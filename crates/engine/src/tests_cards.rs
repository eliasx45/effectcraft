//! Cards (batch 1, CX1): card.make / card.style / card.saveStyle / card.styles.

use effectcraft_keyframe::Value as KV;
use effectcraft_project::LayerId;
use serde_json::json;

use crate::Session;
use crate::commands::cards;

fn session() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Cards", "width": 1920, "height": 1080, "frameRate": 30, "duration": 4})).unwrap();
    // A 600×600 "picture".
    let a = s.execute("layer.newSolid", json!({"color": "#4b5f8f", "width": 600, "height": 600})).unwrap()["layer"].as_u64().unwrap();
    (s, a)
}

fn card_box(s: &Session, layer: u64) -> Option<(String, f64, f64)> {
    let l = s.active_comp()?.layer(LayerId(layer))?;
    let g = cards::card_mask(l)?;
    let KV::Path(sp) = &g.get("path")?.value else { return None };
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for v in &sp.vertices {
        x0 = x0.min(v[0]);
        x1 = x1.max(v[0]);
        y0 = y0.min(v[1]);
        y1 = y1.max(v[1]);
    }
    Some((g.name.clone(), x1 - x0, y1 - y0))
}

#[test]
fn make_card_is_a_centred_two_by_three_mask_in_one_undo_step() {
    let (mut s, a) = session();
    let before = s.history.undo.len();
    let r = s.execute("card.make", json!({"layers": [a]})).unwrap();
    assert_eq!(r["cards"][0]["layer"], json!(a));
    assert_eq!(s.history.undo.len(), before + 1, "one undo step");
    let (name, w, h) = card_box(&s, a).unwrap();
    assert_eq!(name, "Card · Devin 2:3 · 8.6 %");
    // 2:3 inside a square: full height, two thirds of the width (rounded corners keep the box).
    assert!((h - 600.0).abs() < 1.0, "{h}");
    assert!((w - 400.0).abs() < 1.0, "{w}");
    // Devin's hairline: a Stroke style, white, 32 %, 1 px (the layer is at 100 %), inside.
    let l = s.active_comp().unwrap().layer(LayerId(a)).unwrap();
    let stroke = l.layer_styles().and_then(|g| g.sub("stroke")).expect("the card's hairline");
    assert_eq!(stroke.get("opacity").unwrap().value, KV::Scalar(cards::HAIRLINE_OPACITY));
    assert_eq!(stroke.get("size").unwrap().value, KV::Scalar(1.0));
    assert_eq!(stroke.get("position").unwrap().value, KV::Enum(1));
    assert_eq!(r["cards"][0]["hairline"], json!(true));
    s.execute("edit.undo", json!({})).unwrap();
    assert!(card_box(&s, a).is_none());
}

#[test]
fn restyle_reshapes_the_same_mask() {
    let (mut s, a) = session();
    s.execute("card.make", json!({"layers": [a]})).unwrap();
    s.execute("card.style", json!({"layers": [a], "style": "square"})).unwrap();
    let (name, w, h) = card_box(&s, a).unwrap();
    assert_eq!(name, "Card · Square 1:1");
    assert!((w - h).abs() < 1.0);
    let masks = s.active_comp().unwrap().layer(LayerId(a)).unwrap().masks().unwrap().children.len();
    assert_eq!(masks, 1, "restyled, not a second mask");
    // Still one hairline.
    let l = s.active_comp().unwrap().layer(LayerId(a)).unwrap();
    assert_eq!(l.layer_styles().unwrap().groups().filter(|g| g.match_id == "stroke").count(), 1);
    // A layer that isn't a card can't be restyled.
    let b = s.execute("layer.newSolid", json!({"color": "#000000", "width": 100, "height": 100})).unwrap()["layer"].as_u64().unwrap();
    assert!(s.execute("card.style", json!({"layers": [b], "style": "reel"})).is_err());
    assert!(s.execute("card.make", json!({"layers": [a], "style": "nope"})).is_err());
}

#[test]
fn saved_style_shows_in_the_list_and_applies() {
    let (mut s, a) = session();
    s.execute("card.make", json!({"layers": [a], "style": "round"})).unwrap();
    let r = s.execute("card.saveStyle", json!({"layer": a, "name": "My round"})).unwrap();
    let id = r["style"]["id"].as_str().unwrap().to_string();
    let list = s.execute("card.styles", json!({})).unwrap();
    assert!(list["styles"].as_array().unwrap().iter().any(|st| st["id"] == json!(id) && st["saved"] == json!(true)));
    s.execute("card.style", json!({"layers": [a], "style": "My round"})).unwrap();
    assert_eq!(card_box(&s, a).unwrap().0, "Card · My round");
    // The same name twice is refused.
    assert!(s.execute("card.saveStyle", json!({"layer": a, "name": "My round"})).is_err());
}

#[test]
fn scale_layer_scales_value_and_keys() {
    let (mut s, a) = session();
    s.execute("prop.set", json!({"layer": a, "path": "transform/scale", "value": [50, 50]})).unwrap();
    let mut l = s.active_comp().unwrap().layer(LayerId(a)).unwrap().clone();
    cards::scale_layer(&mut l, 2.0);
    let v = l.props.prop("transform/scale").unwrap().value.clone();
    match v {
        KV::Vec2(x) => assert!((x[0] - 100.0).abs() < 1e-6),
        KV::Vec3(x) => assert!((x[0] - 100.0).abs() < 1e-6),
        other => panic!("{other:?}"),
    }
}
