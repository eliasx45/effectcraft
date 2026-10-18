//! Library variants with situation cards: save from the selection, list, rank by situation, the
//! sidecar's round trip beside an untouched library.json, card updates, applying with undo, and
//! junk cards or a damaged sidecar refused without a crash.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::Session;
use crate::library_variants::{self as lv, SituationCard};

fn out_dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-out").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A library folder listing two Devin Jatho scenes (their files needn't exist to keep variants).
fn library(root: &Path) -> PathBuf {
    let lib = root.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let index = json!({"format": "effectcraft-library", "version": 1, "name": "Test Library", "collections": {"order": {"Devin Jatho": 1}},
        "entries": [
            {"id": "dj1", "name": "DJ1 Trial Rings", "category": "Devin Jatho/Scenes", "kind": "comp", "file": "projects/dj1.ecproj", "comp": "DJ1"},
            {"id": "zoom", "name": "YouTube Zoom Out", "category": "Devin Jatho/Scenes", "kind": "comp", "file": "projects/zoom.ecproj", "comp": "Zoom"}
        ],
        "catalog_only": []});
    std::fs::write(lib.join("library.json"), serde_json::to_vec_pretty(&index).unwrap()).unwrap();
    lib
}

/// A session with `lib` added and a `w`×`h` comp holding a 400×300 solid, selected.
fn session(lib: &Path, w: u32, h: u32) -> (Session, u64) {
    let mut s = Session::default();
    s.execute("library.addFolder", json!({"path": lib.to_string_lossy()})).unwrap();
    s.execute("comp.new", json!({"name": "Edit", "width": w, "height": h, "frameRate": 30, "duration": 4})).unwrap();
    let layer = s.execute("layer.newSolid", json!({"name": "Ring", "width": 400, "height": 300, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.select", json!({"layers": [layer]})).unwrap();
    (s, layer)
}

#[test]
fn save_list_match_update_and_round_trip() {
    let root = out_dir("library-variants");
    let lib = library(&root);
    let index_before = std::fs::read(lib.join("library.json")).unwrap();

    // A phone-shot variant in a 16:9 comp: the box, comp size and duration come from the document.
    let (mut s, _) = session(&lib, 1920, 1080);
    let a = s
        .execute_checked(
            "library.saveVariant",
            json!({"parent": "dj1", "card": {"text": "what these pages have in common", "word": "common", "shot": "phone", "energy": 0.8, "ease": "Devin ease 3", "why": "too fast, wrong word"}}),
        )
        .unwrap();
    assert_eq!((a["parent"].as_str(), a["kind"].as_str(), a["count"].as_u64()), (Some("dj1"), Some("layers"), Some(1)), "{a}");
    assert!(Path::new(a["file"].as_str().unwrap()).is_file(), "{a}");
    assert_eq!(a["card"]["compSize"], json!([1920.0, 1080.0]));
    let box_ = a["card"]["layer"].as_array().unwrap();
    assert_eq!((box_[2].as_f64(), box_[3].as_f64()), (Some(400.0), Some(300.0)), "{a}");
    assert!((a["card"]["aspect"].as_f64().unwrap() - 16.0 / 9.0).abs() < 1e-9);
    assert_eq!(a["card"]["shot"], "phone");
    assert_eq!(a["card"]["wordCategory"], "word");
    assert_eq!(a["card"]["why"], json!(["too fast", "wrong word"]));
    assert!(a["card"]["duration"].as_f64().unwrap() > 0.0);
    assert!(a["card"]["created"].as_u64().unwrap() > 0);
    assert!(a["summary"].as_str().unwrap().contains("phone · "), "{a}");

    // A talking-head variant in a vertical comp, about money.
    let (mut v, _) = session(&lib, 1080, 1920);
    let b = v
        .execute("library.saveVariant", json!({"parent": "dj1", "name": "DJ1 vertical", "card": {"word": "$10k", "shot": "talking head", "energy": "low"}}))
        .unwrap();
    assert_eq!((b["name"].as_str(), b["card"]["wordCategory"].as_str(), b["card"]["energy"].as_f64()), (Some("DJ1 vertical"), Some("money"), Some(0.2)));
    assert_eq!(b["card"]["shot"], "talkingHead");

    let list = s.execute("library.variants", json!({"parent": "dj1"})).unwrap();
    assert_eq!(list["count"], 2, "{list}");
    let counts = s.execute("library.variants", json!({})).unwrap();
    assert_eq!((counts["counts"]["dj1"].as_u64(), counts["total"].as_u64()), (Some(2), Some(2)), "{counts}");
    assert_eq!(s.execute("library.variants", json!({"parent": "zoom"})).unwrap()["count"], 0);

    // Ranking: a 16:9 phone shot with high energy finds the phone variant first, and says why.
    let m =
        s.execute_checked("library.matchVariant", json!({"situation": {"shot": "phone", "compSize": [1920, 1080], "energy": 0.7, "word": "common"}})).unwrap();
    assert_eq!(m["considered"], 2);
    assert_eq!(m["matches"][0]["id"], a["id"], "{m}");
    let matched: Vec<&str> = m["matches"][0]["matched"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(["shot", "aspect", "energy", "wordCategory"].iter().all(|f| matched.contains(f)), "{matched:?}");
    assert!(m["matches"][0]["score"].as_f64().unwrap() > m["matches"][1]["score"].as_f64().unwrap());
    // …and a vertical talking head about money finds the other.
    let m2 = s.execute("library.matchVariant", json!({"situation": {"shot": "talkingHead", "aspect": 0.5625, "word": "$500"}, "limit": 1})).unwrap();
    assert_eq!((m2["matches"][0]["id"].as_str(), m2["matches"].as_array().unwrap().len()), (b["id"].as_str(), 1), "{m2}");
    // From the selection: the 16:9 comp and 400×300 box (no shot) still prefer the 16:9 variant.
    let m3 = s.execute("library.matchVariant", json!({"fromSelection": true, "parent": "dj1"})).unwrap();
    assert_eq!(m3["matches"][0]["id"], a["id"], "{m3}");
    assert!(s.execute("library.matchVariant", json!({"situation": {}})).is_err(), "nothing to match on");

    // Round trip: variants.json beside an untouched library.json, read back by a new session.
    assert_eq!(std::fs::read(lib.join("library.json")).unwrap(), index_before, "library.json must not change");
    let side: Value = serde_json::from_slice(&std::fs::read(lib.join(lv::SIDECAR)).unwrap()).unwrap();
    assert_eq!((side["format"].as_str(), side["version"].as_u64()), (Some("effectcraft-library-variants"), Some(1)));
    assert_eq!(side["variants"].as_array().unwrap().len(), 2);
    assert!(side["variants"][0]["file"].as_str().unwrap().starts_with("variants/dj1/"), "relative to the library: {side}");
    let fresh = Session::default();
    let back = lv::read(&fresh, &lib.to_string_lossy()).unwrap();
    assert_eq!(back.len(), 2);
    assert_eq!(serde_json::to_value(&back[0].card).unwrap(), a["card"]);
    // The library still reads as a library.
    assert_eq!(s.execute("library.folders", json!({})).unwrap()["libraries"][0]["entries"], 2);

    // Update a card: new energy, an added why tag; the rest stays.
    let id = a["id"].as_str().unwrap();
    let u = s.execute_checked("library.updateCard", json!({"id": id, "card": {"energy": 0.3, "note": "slower in"}, "addWhy": ["not my style"]})).unwrap();
    assert_eq!(u["card"]["energy"], 0.3);
    assert_eq!(u["card"]["why"], json!(["too fast", "wrong word", "not my style"]));
    assert_eq!((u["card"]["shot"].as_str(), u["card"]["created"].as_u64()), (Some("phone"), a["card"]["created"].as_u64()));
    assert!(u["card"]["updated"].as_u64() >= a["card"]["updated"].as_u64());
    let stored = lv::read(&fresh, &lib.to_string_lossy()).unwrap();
    assert_eq!(stored.iter().find(|v| v.id == id).unwrap().card.note, "slower in");
    assert!(s.execute("library.updateCard", json!({"id": "nope", "card": {}})).is_err());

    // Apply a variant: its layer comes in, one undo step.
    let before = s.project.comp(s.active_comp_id().unwrap()).unwrap().layers.len();
    let r = s.execute("library.applyVariant", json!({"id": id, "time": 1.0})).unwrap();
    assert_eq!(r["result"]["layers"].as_array().map(Vec::len), Some(1), "{r}");
    assert_eq!(s.project.comp(s.active_comp_id().unwrap()).unwrap().layers.len(), before + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.comp(s.active_comp_id().unwrap()).unwrap().layers.len(), before);
}

/// A selected precomp layer (an applied library comp, refined) saves its comp, nested again on apply.
#[test]
fn precomp_variants_keep_the_comp() {
    let root = out_dir("library-variants-precomp");
    let lib = library(&root);
    let (mut s, layer) = session(&lib, 1920, 1080);
    s.execute("layer.precompose", json!({"layers": [layer], "name": "DJ1 refined", "mode": "move"})).unwrap();
    let pre = s.state.selected_layers.first().copied().map(|l| l.0).unwrap_or(layer);
    s.execute("layer.select", json!({"layers": [pre]})).unwrap();
    let r = s.execute("library.saveVariant", json!({"parent": "zoom", "card": {"shot": "overlay"}})).unwrap();
    assert_eq!((r["kind"].as_str(), r["comp"].as_str()), (Some("comp"), Some("DJ1 refined")), "{r}");
    let applied = s.execute("library.applyVariant", json!({"id": r["id"]})).unwrap();
    assert!(applied["result"]["layer"].as_u64().is_some(), "{applied}");
}

/// Junk cards are refused with a message and nothing is written; a damaged sidecar is reported,
/// never overwritten.
#[test]
fn junk_cards_and_damaged_sidecars_are_refused() {
    let root = out_dir("library-variants-junk");
    let lib = library(&root);
    let (mut s, _) = session(&lib, 1920, 1080);
    for junk in [
        json!("garbage"),
        json!(42),
        json!({"shot": "spaceship"}),
        json!({"energy": 7}),
        json!({"energy": "loud"}),
        json!({"layer": [0, 0, -5, 3]}),
        json!({"layer": "big"}),
        json!({"compSize": [0, 1080]}),
        json!({"duration": -1}),
        json!({"why": 5}),
        json!({"why": [1, 2]}),
        json!({"text": ["not", "text"]}),
        json!({"word": "x".repeat(500)}),
        json!({"bogus": 1}),
        json!({"accentFrame": "soon"}),
    ] {
        let e = s.execute("library.saveVariant", json!({"parent": "dj1", "card": junk.clone()}));
        assert!(e.is_err(), "accepted {junk}");
        assert!(SituationCard::from_input(&junk).is_err(), "parsed {junk}");
    }
    assert!(!lib.join(lv::SIDECAR).exists(), "nothing written for junk");
    assert!(!lib.join(lv::FOLDER).exists());
    assert!(s.execute("library.saveVariant", json!({"parent": "nope"})).unwrap_err().to_string().contains("no library entry"));
    assert!(s.execute("library.saveVariant", json!({})).is_err());
    assert!(s.execute_checked("library.saveVariant", json!({"parent": "dj1", "cards": {}})).is_err(), "unknown parameter");

    // A good save, then the sidecar gets damaged: listed as an error, saving refuses, file kept.
    s.execute("library.saveVariant", json!({"parent": "dj1"})).unwrap();
    std::fs::write(lib.join(lv::SIDECAR), b"{ not json").unwrap();
    let list = s.execute("library.variants", json!({"parent": "dj1"})).unwrap();
    assert_eq!((list["count"].as_u64(), list["errors"].as_array().map(Vec::len)), (Some(0), Some(1)), "{list}");
    assert!(s.execute("library.saveVariant", json!({"parent": "dj1"})).unwrap_err().to_string().contains("damaged"));
    assert_eq!(std::fs::read(lib.join(lv::SIDECAR)).unwrap(), b"{ not json");
    assert!(s.execute("library.matchVariant", json!({"situation": {"shot": "phone"}})).unwrap()["matches"].as_array().unwrap().is_empty());
}

#[test]
fn word_categories_and_similarity() {
    assert_eq!(lv::word_category("$10k"), "money");
    assert_eq!(lv::word_category("3x"), "number");
    assert_eq!(lv::word_category("NEVER"), "negation");
    assert_eq!(lv::word_category("today"), "time");
    assert_eq!(lv::word_category("everything"), "intensifier");
    assert_eq!(lv::word_category("you"), "person");
    assert_eq!(lv::word_category("content"), "word");
    let q = SituationCard::from_input(&json!({"shot": "phone", "duration": 1.0})).unwrap();
    let same = SituationCard::from_input(&json!({"shot": "phone", "duration": 1.0})).unwrap();
    let unknown = SituationCard::default();
    let other = SituationCard::from_input(&json!({"shot": "overlay", "duration": 3.0})).unwrap();
    let (s1, m1, _) = lv::similarity(&q, &same);
    let (s2, m2, f2) = lv::similarity(&q, &unknown);
    let (s3, _, _) = lv::similarity(&q, &other);
    assert_eq!((s1, m1), (1.0, vec!["shot", "duration"]));
    assert_eq!((s2, m2.len(), f2["shot"].clone()), (0.5, 0, Value::Null), "unknown counts half, matches nothing");
    assert_eq!(s3, 0.0);
}
