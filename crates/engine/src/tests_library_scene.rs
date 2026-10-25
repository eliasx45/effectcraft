//! `library.scene`: a scene card found by a loose name, placed on a word of the speech map, its
//! texts and footage swapped (also inside its precomps), one undo step, and the errors.

use std::sync::Arc;

use effectcraft_project::speech::{SpeechMap, SpeechWord};
use effectcraft_project::{Footage, FootageKind, ItemKind, LayerSource};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;

fn out_dir(name: &str) -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-out").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn footage(s: &mut Session, name: &str) -> u64 {
    let p = Arc::make_mut(&mut s.project);
    let f = Footage { path: format!("/nowhere/{name}"), kind: FootageKind::Still, width: 64, height: 36, has_video: true, ..Default::default() };
    p.add_item(name, Default::default(), None, ItemKind::Footage(f)).0
}

/// A library folder with the scene “YT Zoom Out” (a title, and a precomp “Card” holding a
/// channel name and a thumbnail) and an unrelated card.
fn library(dir: &std::path::Path) {
    let mut s = Session::default();
    let thumb = footage(&mut s, "thumb.png");
    s.execute("comp.new", json!({"name": "Card", "width": 64, "height": 36, "frameRate": 30, "duration": 2})).unwrap();
    let card = s.active_comp_id().unwrap();
    s.execute("layer.newText", json!({"name": "Channel", "text": "Devin Jatho", "size": 8})).unwrap();
    s.execute("layer.addItem", json!({"comp": card.0, "item": thumb})).unwrap();
    s.execute("comp.new", json!({"name": "YT Zoom Out", "width": 160, "height": 90, "frameRate": 30, "duration": 3})).unwrap();
    let main = s.active_comp_id().unwrap();
    s.execute("layer.newText", json!({"name": "Title", "text": "1M views", "size": 12})).unwrap();
    s.execute("layer.addItem", json!({"comp": main.0, "item": card.0})).unwrap();
    s.execute("file.saveAs", json!({"path": dir.join("scenes.ecproj").to_string_lossy()})).unwrap();
    let index = json!({
        "format": "effectcraft-library", "version": 1, "name": "Devin",
        "entries": [
            {"id": "yt", "name": "YT Zoom Out", "kind": "comp", "file": "scenes.ecproj", "comp": "YT Zoom Out", "category": "Devin Jatho/1 Scenes"},
            {"id": "card", "name": "Card", "kind": "comp", "file": "scenes.ecproj", "comp": "Card", "category": "Devin Jatho/2 Elements"}
        ]
    });
    std::fs::write(dir.join("library.json"), serde_json::to_vec(&index).unwrap()).unwrap();
}

fn session(dir: &std::path::Path) -> Session {
    let mut s = Session::default();
    s.execute("library.addFolder", json!({"path": dir.to_string_lossy()})).unwrap();
    s.execute("comp.new", json!({"name": "Edit", "width": 160, "height": 90, "frameRate": 30, "duration": 10})).unwrap();
    s
}

fn text_of(s: &Session, comp: u64, layer: u64) -> String {
    let c = s.project.comp(effectcraft_project::ItemId(comp)).unwrap();
    match c.layer(effectcraft_project::LayerId(layer)).unwrap().props.prop("text/sourceText").map(|p| &p.value) {
        Some(effectcraft_keyframe::Value::Text(d)) => d.text.clone(),
        _ => String::new(),
    }
}

fn slot<'a>(r: &'a Value, name: &str) -> &'a Value {
    r["slots"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap_or_else(|| panic!("no slot {name}: {r:#}"))
}

#[test]
fn scene_by_loose_name_on_a_word_with_swaps() {
    let dir = out_dir("library-scene");
    library(&dir);
    let mut s = session(&dir);
    let edit = s.active_comp_id().unwrap();
    let cam = footage(&mut s, "Tyson Cam");
    let words = ["so", "this", "video", "got", "views"]
        .iter()
        .enumerate()
        .map(|(i, w)| SpeechWord {
            word: w.to_string(),
            start: Tick::from_seconds_f64(1.0 + i as f64 * 0.5),
            end: Tick::from_seconds_f64(1.4 + i as f64 * 0.5),
            ..Default::default()
        })
        .collect();
    Arc::make_mut(&mut s.project).comp_mut(edit).unwrap().speech = Some(Arc::new(SpeechMap { words, ..Default::default() }));
    let undo_before = s.history.undo.len();

    let r = s
        .execute(
            "library.scene",
            json!({"scene": "yt zoom-out", "word": "video", "swap": {"Title": "4.2M views", "channel": "Tyson", "thumb.png": "Tyson Cam"}}),
        )
        .unwrap();
    assert_eq!((r["scene"]["id"].as_str(), r["at"].as_f64(), r["comp"].as_u64()), (Some("yt"), Some(2.0), Some(edit.0)), "{r:#}");
    // The scene starts on “video” (2.0 s).
    let layer = r["layer"].as_u64().unwrap();
    let l = s.project.comp(edit).unwrap().layer(effectcraft_project::LayerId(layer)).unwrap();
    assert_eq!(l.start_time.seconds(), 2.0);
    // Texts swapped, in the scene and its precomp; the thumbnail now shows the cam.
    let title = slot(&r, "Title");
    assert_eq!(text_of(&s, title["comp"].as_u64().unwrap(), title["layer"].as_u64().unwrap()), "4.2M views");
    let channel = slot(&r, "Channel");
    assert_eq!(text_of(&s, channel["comp"].as_u64().unwrap(), channel["layer"].as_u64().unwrap()), "Tyson");
    let thumb = slot(&r, "thumb.png");
    assert_eq!((thumb["kind"].as_str(), thumb["now"].as_str()), (Some("footage"), Some("Tyson Cam")), "{thumb}");
    let tl = s
        .project
        .comp(effectcraft_project::ItemId(thumb["comp"].as_u64().unwrap()))
        .unwrap()
        .layer(effectcraft_project::LayerId(thumb["layer"].as_u64().unwrap()))
        .unwrap();
    assert!(matches!(tl.source, LayerSource::Footage { item } if item.0 == cam));
    assert_eq!(r["swapped"].as_array().unwrap().len(), 3, "{r:#}");
    assert_eq!(r["unmatched"], json!([]));
    // One undo step takes the whole scene out.
    assert_eq!(s.history.undo.len(), undo_before + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.comp(edit).unwrap().layers.is_empty());
}

#[test]
fn scene_list_and_errors() {
    let dir = out_dir("library-scene-errors");
    library(&dir);
    let mut s = session(&dir);
    // No scene: the scenes to pick from (cards in a Scenes folder).
    let r = s.execute("library.scene", json!({})).unwrap();
    assert_eq!(r["scenes"], json!([{"id": "yt", "name": "YT Zoom Out", "category": "Devin Jatho/1 Scenes"}]));
    // A swap that names nothing lists the slots and changes nothing.
    let e = s.execute("library.scene", json!({"scene": "YT Zoom Out", "at": 1, "swap": {"Headline": "x"}})).unwrap_err().to_string();
    assert!(e.contains("no layer of “YT Zoom Out” is named “Headline”") && e.contains("Title (text)") && e.contains("Channel (text)"), "{e}");
    assert!(s.project.comp(s.active_comp_id().unwrap()).unwrap().layers.is_empty());
    // A word the speech map doesn't have (here: no map at all), and an unknown scene.
    let e = s.execute("library.scene", json!({"scene": "YT Zoom Out", "word": "views"})).unwrap_err().to_string();
    assert!(e.contains("no speech map"), "{e}");
    let e = s.execute("library.scene", json!({"scene": "Spinning Globe"})).unwrap_err().to_string();
    assert!(e.contains("no Motion Library scene matches “Spinning Globe”") && e.contains("YT Zoom Out"), "{e}");
}
