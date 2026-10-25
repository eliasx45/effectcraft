//! Editing harness step 2 ("senses"): timecode next to seconds and accepted as input, ease
//! readout per key, the who-did-what feed and agent notices.

use std::sync::Arc;

use effectcraft_keyframe::{Ease, Eases, Interp};
use effectcraft_project::{ItemId, LayerId};
use serde_json::{Value, json};

use crate::Session;

fn id(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap()
}

/// Comp "Main" (30 fps, 70 s) with a solid "Card".
fn setup() -> (Session, ItemId, LayerId) {
    let mut s = Session::default();
    let c = ItemId(id(&s.execute("comp.new", json!({"name": "Main", "duration": 70, "frameRate": 30})).unwrap(), "comp"));
    let l = LayerId(id(&s.execute("layer.newSolid", json!({"name": "Card"})).unwrap(), "layer"));
    (s, c, l)
}

#[test]
fn timecode_reads_as_the_timeline_shows_it() {
    let (s, c, _) = setup();
    let comp = s.project.comp(c).unwrap();
    let tc = |secs: f64| crate::readout::short_timecode(comp, comp.frame_rate.tick_of((secs * 30.0).round() as i64));
    assert_eq!(tc(200.0 / 30.0), "6:20");
    assert_eq!(tc(5.0 / 30.0), "0:05");
    assert_eq!(tc(62.5), "1:02:15");
}

#[test]
fn time_params_take_timecode_and_frames() {
    let (mut s, c, l) = setup();
    for (time, want) in [(json!("6:20"), 200), (json!("0:00:01:00"), 30), (json!({"frame": 45}), 45), (json!("12f"), 12), (json!("0.5"), 15)] {
        s.execute("prop.addKey", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity", "time": time, "value": 50})).unwrap();
        let keys = s.execute("prop.get", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity"})).unwrap()["keys"].clone();
        assert!(keys.as_array().unwrap().iter().any(|k| k["frame"] == want), "{time} → frame {want}: {keys}");
    }
    let e = s.execute("prop.addKey", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity", "time": {"frame": 1.5}})).unwrap_err().to_string();
    assert!(e.contains("whole frame"), "{e}");
    // Lists of keys too, and time.resolve for callers that check numbers first.
    let r = s.execute("time.resolve", json!({"comp": c.0, "params": {"keys": [{"time": "1:00"}], "time": {"frame": 15}, "text": "6:20"}})).unwrap();
    assert_eq!(r["params"]["keys"][0]["time"], 1.0);
    assert_eq!(r["params"]["time"], 0.5);
    assert_eq!(r["params"]["text"], "6:20", "only time parameters are read as times");
}

#[test]
fn keys_show_timecode_and_eases_per_side() {
    let (mut s, c, l) = setup();
    for (t, v) in [(0.0, [0.0, 0.0, 100.0]), (1.0, [100.0, 50.0, 100.0])] {
        s.execute("prop.addKey", json!({"comp": c.0, "layer": l.0, "path": "transform/scale", "time": t, "value": v})).unwrap();
    }
    {
        let comp = Arc::make_mut(&mut s.project).comp_mut(c).unwrap();
        let k = &mut comp.layer_mut(l).unwrap().props.prop_mut("transform/scale").unwrap().keys[1];
        k.in_interp = Interp::Bezier;
        k.out_interp = Interp::Bezier;
        k.in_ease = Eases::splat(Ease { speed: 0.0, influence: 0.75 }, 3);
        k.continuous = false;
    }
    let p = s.execute("prop.get", json!({"comp": c.0, "layer": l.0, "path": "transform/scale", "time": "0:15"})).unwrap();
    assert_eq!(p["tc"], "0:15");
    assert_eq!(p["frame"], 15);
    let k = &p["keys"][1];
    assert_eq!(k["tc"], "1:00");
    assert_eq!(k["frame"], 30);
    assert_eq!(k["inEase"][0]["influence"], 75.0);
    assert_eq!(k["inEase"].as_array().unwrap().len(), 3, "one entry per dimension");
    assert_eq!(k["continuous"], false);
    assert!(k["ease"].as_str().unwrap().starts_with("in bezier 0/s 75%"), "{k}");
    assert!(k["outEase"].is_null(), "the last key has no out side: {k}");
    assert!(p["keys"][0].get("continuous").is_none(), "linear keys have no handles");
}

#[test]
fn feed_says_who_changed_what_in_words() {
    let (mut s, c, l) = setup();
    let r0 = s.revision;
    s.execute("prop.addKey", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity", "time": "6:20", "value": 40, "client": "bot"})).unwrap();
    s.execute("prop.set", json!({"comp": c.0, "layer": l.0, "path": "transform/rotation", "value": 15})).unwrap();
    let f = s.execute("history.feed", json!({"since": r0, "client": "bot"})).unwrap();
    let e = f["entries"].as_array().unwrap();
    assert_eq!(e.len(), 2, "{f}");
    assert_eq!((e[0]["by"].as_str(), e[0]["who"].as_str()), (Some("bot"), Some("you")));
    assert!(e[0]["changes"][0].as_str().unwrap().contains("Card › Opacity: animated"), "{f}");
    assert_eq!(e[1]["by"], "user");
    assert_eq!(e[1]["changes"][0], "Card › Rotation: 0 → 15");
    // Which comps each change touched (the handover snapshot hook snaps those).
    assert_eq!((&e[0]["comps"], &e[1]["comps"]), (&json!([c.0]), &json!([c.0])), "{f}");
    // Without `since`: only what came after the last read.
    s.execute("prop.set", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity", "time": "6:20", "value": 60})).unwrap();
    let f = s.execute("history.feed", json!({"client": "bot"})).unwrap();
    assert_eq!(f["entries"].as_array().unwrap().len(), 1, "{f}");
    assert_eq!(f["entries"][0]["changes"][0], "Card › Opacity: key @ 6:20 40 → 60");
    // Undo is a change too, by whoever undid.
    s.execute("edit.undo", json!({})).unwrap();
    let f = s.execute("history.feed", json!({"client": "bot"})).unwrap();
    assert!(f["entries"][0]["label"].as_str().unwrap().starts_with("Undo"), "{f}");
}

#[test]
fn notices_tell_an_agent_once_what_others_did_and_about_another_project() {
    let (mut s, c, l) = setup();
    let notice = |s: &mut Session| s.execute("agent.notices", json!({"client": "bot"})).unwrap();
    assert_eq!(notice(&mut s), json!({}), "the first call only marks what the agent has seen");
    s.execute("prop.set", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity", "value": 20, "client": "bot"})).unwrap();
    assert_eq!(notice(&mut s), json!({}), "its own edits are no news");
    s.execute("prop.set", json!({"comp": c.0, "layer": l.0, "path": "transform/opacity", "value": 30})).unwrap();
    let n = notice(&mut s);
    assert!(n["text"].as_str().unwrap().contains("user · Change Property: Card › Opacity: 20 → 30"), "{n}");
    assert_eq!(notice(&mut s), json!({}), "told once");
    assert_eq!(s.execute("agent.notices", json!({})).unwrap(), json!({}), "nothing for the user");
    s.replace_project(effectcraft_project::Project::default(), Some("/x/Other.ecproj".into()));
    let n = notice(&mut s);
    assert_eq!(n["projectChanged"]["to"], "/x/Other.ecproj");
    assert!(n["text"].as_str().unwrap().contains("now Other.ecproj"), "{n}");
}

#[test]
fn contact_sheet_takes_comp_frames() {
    let (mut s, _, _) = setup();
    let v = s.execute("render.contactSheet", json!({"frames": {"from": 10, "to": 16, "every": 3}, "max_side": 200, "base64": true})).unwrap();
    assert_eq!(v["frames"], json!([10, 13, 16]));
    let v = s.execute("render.contactSheet", json!({"frames": [200, 201], "max_side": 200, "base64": true})).unwrap();
    assert_eq!(v["frames"], json!([200, 201]));
    assert!(s.execute("render.contactSheet", json!({"frames": {"from": 5, "to": 1}})).is_err());
}
