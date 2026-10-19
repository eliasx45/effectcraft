//! Text presets (batch 1b DX1, reworked as CX8b/CX8c): text.presetFamilies lists Elias's preset
//! cards of Motion Library ▸ Elias ▸ Text animations with the controls read from their files;
//! text.apply applies one through anim.applyPreset and sets controls by name.
//! The fixtures are a library folder written by the test (library.json + an .ecpreset saved with
//! anim.savePreset), never the real library.
//! Not run yet (batch 1): TODO(batch1-verify) run `cargo test -p effectcraft-engine tests_text_presets`.

use effectcraft_keyframe::{Keyframe, Value as KV};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Expression, ItemId, LayerId};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::Session;
use crate::commands::text_presets::unit_for_tests;

pub(crate) fn temp_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("ec-text-presets-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Adds an Expression Control `effect` ("ec.control.slider"…) named `name` with `value` to layer
/// `l` of comp `cid`; a dropdown gets `items`.
pub(crate) fn add_control(s: &mut Session, cid: ItemId, l: u64, effect: &str, name: &str, value: KV, items: &[&str]) {
    let spec = effectcraft_effects::find(effect).unwrap();
    let (name, items): (String, Vec<String>) = (name.into(), items.iter().map(|i| i.to_string()).collect());
    s.edit("fixture", None, |proj, _| {
        let mut next = proj.next_id;
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), &name, [100.0, 100.0]);
        proj.next_id = next;
        if let Some(effectcraft_project::Node::Prop(pr)) = g.children.first_mut() {
            pr.value = value;
            if !items.is_empty() {
                pr.ui = effectcraft_project::ParamUi::Popup { options: items };
            }
        }
        crate::commands::layer_mut(proj, cid, LayerId(l))?.props.sub_mut("effects").unwrap().children.push(g.into());
        Ok(())
    })
    .unwrap();
}

/// Adds an animator `name` (Opacity 0, a Range Selector Based On `based_on`, Ramp Up) to layer `l`:
/// with `offset_keys` its Offset is keyed −100 → 100 over 0..2 s, else its Offset is
/// `offset_expr`; its Amount is `amount_expr`.
pub(crate) fn add_fade(s: &mut Session, cid: ItemId, l: u64, name: &str, based_on: u32, offset_keys: bool, amount_expr: &str) {
    let (name, amount_expr) = (name.to_string(), amount_expr.to_string());
    s.edit("fixture", None, |proj, _| {
        let mut next = proj.next_id;
        let mut ids = Ids(&mut next);
        let mut opacity = build::text_anim_props(&mut ids, "opacity", false);
        if let Some(o) = opacity.first_mut() {
            o.value = KV::Scalar(0.0);
        }
        let mut g = build::text_animator(&mut ids, &name, opacity);
        proj.next_id = next;
        let rs = g.group_mut("selectors/rangeSelector").unwrap();
        rs.prop_mut("advanced/basedOn").unwrap().value = KV::Enum(based_on);
        rs.prop_mut("advanced/shape").unwrap().value = KV::Enum(1);
        rs.prop_mut("advanced/easeHigh").unwrap().value = KV::Scalar(-100.0);
        rs.prop_mut("advanced/amount").unwrap().expr = Some(Expression { text: amount_expr.clone(), enabled: true });
        let off = rs.prop_mut("offset").unwrap();
        if offset_keys {
            off.keys = vec![Keyframe::new(Tick::ZERO, KV::Scalar(-100.0)), Keyframe::new(Tick::from_seconds_f64(2.0), KV::Scalar(100.0))];
        } else {
            off.expr = Some(Expression { text: "text.animator(\"Fade Up\").selector(\"Range Selector 1\").offset".into(), enabled: true });
        }
        crate::commands::layer_mut(proj, cid, LayerId(l))?.props.group_mut("text/animators").unwrap().children.push(g.into());
        Ok(())
    })
    .unwrap();
}

/// Saves layer `l`'s effects and text animators as `<lib>/assets/<id>/preset.ecpreset` (the way
/// the plug's save_asset runs anim.savePreset), and lists the card in `<lib>/library.json`.
pub(crate) fn save_card(s: &mut Session, lib: &std::path::Path, cards: &mut Vec<Value>, l: u64, id: &str, name: &str, category: &str) {
    let cid = s.active_comp_id().unwrap();
    let layer = s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().clone();
    let mut sel: Vec<(LayerId, u64)> = layer.effects().unwrap().groups().map(|g| (LayerId(l), g.uid)).collect();
    sel.extend(layer.props.group("text/animators").unwrap().groups().map(|g| (LayerId(l), g.uid)));
    s.state.selected_props = sel;
    let dir = lib.join("assets").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("preset.ecpreset");
    s.execute("anim.savePreset", json!({"path": file.to_string_lossy(), "name": name})).unwrap();
    s.state.selected_props.clear();
    cards.push(json!({"id": id, "name": name, "kind": "preset", "category": category, "file": format!("assets/{id}/preset.ecpreset")}));
    let index = json!({"format": "effectcraft-library", "version": 1, "name": "Fixture", "entries": cards});
    std::fs::write(lib.join("library.json"), index.to_string()).unwrap();
    let folder = lib.to_string_lossy().to_string();
    if !s.prefs.libraries.contains(&folder) {
        s.prefs.libraries.push(folder);
    }
}

/// The workshop comp with Elias's "Slide In" shape (controls Travel Distance (px) 150, Direction
/// 90, Animate By [Letters, Words, Full sentence] = Words; animators Fade Up (keyed Offset) and
/// Fade Up · Letters (Offset expression)) saved as a card of a fixture library, and a 25 fps
/// "Main" comp with a speech map ("works" stressed at 1.2 s) and a text layer "Caption".
/// Returns (session, main, caption layer).
fn studio() -> (Session, ItemId, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Creating Text Presets", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let ws = s.active_comp_id().unwrap();
    let t = s.execute("layer.newText", json!({"text": "Slide in", "name": "Slide In"})).unwrap()["layer"].as_u64().unwrap();
    add_control(&mut s, ws, t, "ec.control.slider", "Travel Distance (px)", KV::Scalar(150.0), &[]);
    add_control(&mut s, ws, t, "ec.control.angle", "Direction", KV::Scalar(90.0), &[]);
    add_control(&mut s, ws, t, "ec.control.dropdown", "Animate By", KV::Enum(1), &["Letters", "Words", "Full sentence"]);
    add_fade(&mut s, ws, t, "Fade Up", 2, true, "effect(\"Animate By\")(\"Menu\")==2?100:0");
    add_fade(&mut s, ws, t, "Fade Up · Letters", 0, false, "effect(\"Animate By\")(\"Menu\")==1?100:0");
    let lib = temp_dir("lib");
    let mut cards = vec![];
    save_card(&mut s, &lib, &mut cards, t, "slide-in", "Slide In", "Elias/Text animations");

    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let main = s.active_comp_id().unwrap();
    let voice = s.execute("layer.newSolid", json!({"name": "Voice", "width": 20, "height": 20, "color": "#ffffff"})).unwrap()["layer"].as_u64().unwrap();
    s.execute(
        "speech.import",
        json!({"layer": voice, "map": {"engine": "test", "words": [
            {"word": "this", "start": 0.4, "end": 0.6},
            {"word": "really", "start": 0.7, "end": 1.0},
            {"word": "WORKS", "start": 1.1, "end": 1.5, "syllables": [{"t": 1.1, "vowel": 1.2, "stress": 1}]}
        ]}}),
    )
    .unwrap();
    let cap = s.execute("layer.newText", json!({"text": "Caption", "name": "Caption"})).unwrap()["layer"].as_u64().unwrap();
    (s, main, cap)
}

fn layer(s: &Session, cid: ItemId, l: u64) -> effectcraft_project::Layer {
    s.project.comp(cid).unwrap().layer(LayerId(l)).unwrap().clone()
}

fn control(l: &effectcraft_project::Layer, name: &str) -> KV {
    l.effects().unwrap().groups().find(|g| g.name == name).unwrap().props().next().unwrap().value.clone()
}

fn offset_keys(l: &effectcraft_project::Layer) -> Vec<f64> {
    let g = l.props.group("text/animators").unwrap().groups().find(|g| g.name == "Fade Up").unwrap().clone();
    g.prop("selectors/rangeSelector/offset").unwrap().keys.iter().map(|k| k.time.seconds()).collect()
}

#[test]
fn units_count_like_the_selector_based_on() {
    assert_eq!(unit_for_tests("this really WORKS", "works", 2), (2, 3));
    // Characters count the spaces; the word's middle character.
    assert_eq!(unit_for_tests("ab cd", "cd", 0), (4, 5));
    assert_eq!(unit_for_tests("ab cd", "cd", 1), (3, 4));
    assert_eq!(unit_for_tests("one\r\ntwo words", "words", 3), (1, 2));
    // A word that isn't there is the first unit; nothing panics on empty text.
    assert_eq!(unit_for_tests("ab cd", "zz", 2), (0, 2));
    assert_eq!(unit_for_tests("", "zz", 0), (0, 1));
}

#[test]
fn families_list_the_cards_with_the_controls_from_their_files() {
    let (mut s, _, _) = studio();
    let r = s.execute("text.presetFamilies", json!({})).unwrap();
    assert_eq!(r["category"], "Elias/Text animations");
    let p = &r["presets"][0];
    assert_eq!(p["name"], "Slide In", "{r:#}");
    assert_eq!(p["source"], "library");
    let names: Vec<&str> = p["controls"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(names, ["Travel Distance (px)", "Direction", "Animate By"]);
    assert_eq!(p["controls"][0]["type"], "slider");
    assert_eq!(p["controls"][0]["value"], json!(150.0));
    assert_eq!(p["controls"][2]["type"], "dropdown");
    assert_eq!(p["controls"][2]["value"], "Words");
    assert_eq!(p["controls"][2]["options"], json!(["Letters", "Words", "Full sentence"]));
    assert_eq!(p["animators"], json!(["Fade Up", "Fade Up · Letters"]));
    // Another category has none.
    let r = s.execute("text.presetFamilies", json!({"category": "Elias/Nothing here"})).unwrap();
    assert_eq!(r["presets"], json!([]));
}

#[test]
fn apply_brings_the_preset_and_sets_controls_by_name_in_one_undo_step() {
    let (mut s, main, cap) = studio();
    let before = s.history.undo.len();
    let r = s
        .execute_checked("text.apply", json!({"preset": "slide in", "layers": [cap], "controls": {"Travel Distance (px)": 40, "Animate By": "Letters"}}))
        .unwrap();
    assert_eq!(r["preset"]["name"], "Slide In");
    assert_eq!(s.history.undo.len(), before + 1, "one undo step: {r:#}");
    let l = layer(&s, main, cap);
    assert_eq!(control(&l, "Travel Distance (px)"), KV::Scalar(40.0));
    assert_eq!(control(&l, "Direction"), KV::Scalar(90.0), "untouched controls keep the preset's value");
    assert_eq!(control(&l, "Animate By"), KV::Enum(0));
    // The animators, with the Offset keys from the layer's in point (0).
    let names: Vec<String> = l.props.group("text/animators").unwrap().groups().map(|g| g.name.clone()).collect();
    assert_eq!(names, ["Fade Up", "Fade Up · Letters"]);
    let k = offset_keys(&l);
    assert!(k[0].abs() < 1e-6 && (k[1] - 2.0).abs() < 1e-6, "{k:?}");
    let copy = l.props.group("text/animators").unwrap().groups().find(|g| g.name == "Fade Up · Letters").unwrap().clone();
    assert!(copy.prop("selectors/rangeSelector/offset").unwrap().expr.as_ref().unwrap().text.contains("text.animator(\"Fade Up\")"));
    // Applying again replaces the animators and effects instead of stacking them.
    s.execute_checked("text.apply", json!({"preset": "Slide In", "layers": [cap]})).unwrap();
    let l = layer(&s, main, cap);
    assert_eq!(l.props.group("text/animators").unwrap().groups().count(), 2);
    assert_eq!(l.effects().unwrap().groups().count(), 3);
    s.execute("edit.undo", json!({})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(layer(&s, main, cap).props.group("text/animators").unwrap().groups().count(), 0);
}

#[test]
fn distance_sets_the_travel_controls_and_skips_presets_without_them() {
    let (mut s, main, cap) = studio();
    s.execute_checked("text.apply", json!({"preset": "Slide In", "layers": [cap], "distance": "near"})).unwrap();
    assert_eq!(control(&layer(&s, main, cap), "Travel Distance (px)"), KV::Scalar(40.0));
    // A card with no Travel control: distance does nothing, and it isn't an error.
    let ws = s.project.items.values().find(|i| i.name == "Creating Text Presets").unwrap().id;
    s.open_comp(ws);
    let t = s.execute("layer.newText", json!({"text": "Fade", "name": "Fade"})).unwrap()["layer"].as_u64().unwrap();
    add_fade(&mut s, ws, t, "Fade Up", 2, true, "100");
    add_control(&mut s, ws, t, "ec.control.slider", "Blur In (px)", KV::Scalar(8.0), &[]);
    let lib = std::path::PathBuf::from(s.prefs.libraries.first().unwrap());
    let mut cards: Vec<Value> =
        serde_json::from_slice::<Value>(&std::fs::read(lib.join("library.json")).unwrap()).unwrap()["entries"].as_array().unwrap().clone();
    save_card(&mut s, &lib, &mut cards, t, "plain-fade", "Plain Fade", "Elias/Text animations");
    s.open_comp(main);
    let r = s.execute_checked("text.apply", json!({"preset": "Plain Fade", "layers": [cap], "distance": "far", "controls": {"Blur In (px)": 3}})).unwrap();
    assert_eq!(control(&layer(&s, main, cap), "Blur In (px)"), KV::Scalar(3.0), "a control nobody hardcoded: {r:#}");
}

#[test]
fn sync_makes_a_layer_for_the_words_with_the_reveal_on_the_stressed_syllable() {
    let (mut s, main, _) = studio();
    let r = s.execute_checked("text.apply", json!({"preset": "Slide In", "words": "works", "sync": true})).unwrap();
    let rep = &r["layers"][0];
    assert_eq!(rep["created"], json!(true), "{r:#}");
    assert_eq!(rep["sync"]["by"], "reveal");
    assert!(rep["sync"]["offsetFrames"].as_f64().unwrap().abs() <= 0.6, "{r:#}");
    let lid = rep["layer"].as_u64().unwrap();
    // One word = one unit, half revealed when the Offset (−100 → 100 over 2 s) is at 0, 1 s in;
    // the accent is the vowel (1.2 s) − 1 frame = 1.16 s, so the keys start at 0.16 s (frame 4).
    let l = layer(&s, main, lid);
    assert_eq!(l.name, "WORKS");
    let k = offset_keys(&l);
    assert!((k[0] * 25.0 - 4.0).abs() < 0.01, "{k:?} {r:#}");
    assert!(l.in_point.seconds() <= k[0] + 1e-6 && l.out_point.seconds() >= 1.5);
}

#[test]
fn words_as_a_speech_map_range_work_like_the_phrase() {
    let (mut s, _, _) = studio();
    let r = s.execute_checked("text.apply", json!({"preset": "Slide In", "words": [1, 2], "word": "works", "sync": true, "distance": ""})).unwrap();
    assert_eq!(r["layers"][0]["name"], "really WORKS", "{r:#}");
    assert_eq!(r["layers"][0]["sync"]["word"], "WORKS");
}

#[test]
fn unknown_presets_controls_and_bad_params_are_errors_not_crashes() {
    let (mut s, _, cap) = studio();
    let e = s.execute_checked("text.apply", json!({"preset": "T3b", "layers": [cap]})).unwrap_err().to_string();
    assert!(e.contains("Slide In"), "lists what there is: {e}");
    let e = s.execute_checked("text.apply", json!({"preset": "Slide In", "layers": [cap], "controls": {"Travel": 3}})).unwrap_err().to_string();
    assert!(e.contains("Travel Distance (px)"), "lists the preset's controls: {e}");
    assert!(s.execute_checked("text.apply", json!({"preset": "Slide In", "layers": [cap], "controls": {"Animate By": "Sideways"}})).is_err());
    assert!(s.execute_checked("text.apply", json!({"preset": "Slide In", "layers": [cap], "controls": [1, 2]})).is_err());
    assert!(s.execute_checked("text.apply", json!({"preset": "Slide In", "layers": [cap], "distance": "miles"})).is_err());
    assert!(s.execute_checked("text.apply", json!({"preset": "Slide In", "words": [99, 120]})).is_err());
    assert!(s.execute_checked("text.apply", json!({"preset": "/no/such/file.ecpreset", "layers": [cap]})).is_err());
    assert!(s.execute_checked("text.apply", json!({"layers": [cap]})).is_err());
    // A failed apply leaves the layer as it was.
    let main = s.active_comp_id().unwrap();
    assert_eq!(layer(&s, main, cap).props.group("text/animators").unwrap().groups().count(), 0);
}
