//! PROP-REMOVE: deleting one property of a text animator's Properties (Fill Color, Blur,
//! Tracking…) with `prop.removeGroup` or Edit ▸ Clear, as After Effects' Delete and
//! `Property.remove()` do. Sets that one Animate entry adds go together (Tracking Type with
//! Tracking Amount, the four Gradient Fill properties); fixed properties are refused.

use effectcraft_project::{LayerId, PropGroup};
use serde_json::{Value, json};

use crate::{KeyRef, Session};

/// A comp with a text layer whose Animator 1 has the properties of `kinds`.
fn text_with_animator(kinds: &[&str]) -> (Session, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "C", "width": 320, "height": 120, "duration": 2})).unwrap();
    let t = s.execute("layer.newText", json!({"text": "ABCD"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.addTextAnimator", json!({"layer": t, "properties": kinds})).unwrap();
    (s, t)
}

fn props(s: &Session, t: u64) -> PropGroup {
    s.active_comp().unwrap().layer(LayerId(t)).unwrap().props.group("text/animators/#1/properties").unwrap().clone()
}

/// Match ids of Animator 1's properties, in order.
fn kinds(s: &Session, t: u64) -> Vec<String> {
    props(s, t).props().map(|p| p.match_id.clone()).collect()
}

fn uid(s: &Session, t: u64, m: &str) -> u64 {
    props(s, t).get(m).unwrap_or_else(|| panic!("no {m}")).uid
}

fn remove(s: &mut Session, t: u64, prop: u64) -> crate::Result<Value> {
    s.execute("prop.removeGroup", json!({"layer": t, "prop": prop}))
}

#[test]
fn deleting_fill_color_removes_only_it_in_one_undo_step() {
    let (mut s, t) = text_with_animator(&["fillColor", "opacity"]);
    assert_eq!(kinds(&s, t), ["fillColor", "opacity"]);
    let fill = uid(&s, t, "fillColor");
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let r = remove(&mut s, t, fill).unwrap();
    assert_eq!(r["removed"], json!([fill]));
    assert_eq!(kinds(&s, t), ["opacity"], "just Fill Color went");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    assert_eq!(s.active_comp().unwrap().layers.len(), 1, "the layer stays");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before, "undo restores Fill Color");
    // Blur and Rotation go alone too.
    s.execute("layer.addTextAnimatorProperty", json!({"layer": t, "properties": ["blur", "rotation"]})).unwrap();
    let blur = uid(&s, t, "blur");
    remove(&mut s, t, blur).unwrap();
    assert_eq!(kinds(&s, t), ["fillColor", "opacity", "rotation"]);
}

#[test]
fn tracking_type_and_amount_go_together() {
    let (mut s, t) = text_with_animator(&["tracking", "opacity"]);
    assert_eq!(kinds(&s, t), ["trackingType", "tracking", "opacity"]);
    let before = s.project.clone();
    // Deleting Tracking Amount takes Tracking Type…
    let amount = uid(&s, t, "tracking");
    let r = remove(&mut s, t, amount).unwrap();
    assert_eq!(r["removed"].as_array().map(Vec::len), Some(2));
    assert_eq!(kinds(&s, t), ["opacity"]);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
    // …and deleting Tracking Type takes Tracking Amount.
    let ty = uid(&s, t, "trackingType");
    remove(&mut s, t, ty).unwrap();
    assert_eq!(kinds(&s, t), ["opacity"]);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}

#[test]
fn gradient_fill_properties_go_as_a_set() {
    let (mut s, t) = text_with_animator(&["fillColor", "gradientFill"]);
    assert_eq!(kinds(&s, t), ["fillColor", "gradientType", "gradientStart", "gradientEnd", "gradientFillColors"]);
    let before = s.project.clone();
    for m in effectcraft_project::build::GRADIENT_FILL_PROPS {
        let u = uid(&s, t, m);
        remove(&mut s, t, u).unwrap();
        assert_eq!(kinds(&s, t), ["fillColor"], "deleting {m} removes all four Gradient Fill properties");
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.project, before);
    }
}

#[test]
fn a_shared_companion_stays_while_another_property_needs_it() {
    let (mut s, t) = text_with_animator(&["characterOffset", "characterValue"]);
    assert_eq!(kinds(&s, t), ["characterAlignment", "characterRange", "characterOffset", "characterValue"]);
    let offset = uid(&s, t, "characterOffset");
    remove(&mut s, t, offset).unwrap();
    assert_eq!(kinds(&s, t), ["characterRange", "characterValue"], "Character Range still serves Character Value");
    let value = uid(&s, t, "characterValue");
    remove(&mut s, t, value).unwrap();
    assert!(kinds(&s, t).is_empty());
}

#[test]
fn fixed_properties_and_groups_are_refused() {
    let (mut s, t) = text_with_animator(&["opacity"]);
    let layer = s.active_comp().unwrap().layer(LayerId(t)).unwrap().clone();
    let selector = layer.props.group("text/animators/#1/selectors").unwrap().groups().next().unwrap().clone();
    let refused = [
        ("Range Selector Offset", selector.get("offset").unwrap().uid),
        ("Range Selector Start", selector.get("start").unwrap().uid),
        ("Range Selector Advanced", selector.sub("advanced").unwrap().uid),
        ("Advanced ▸ Amount", selector.sub("advanced").unwrap().get("amount").unwrap().uid),
        ("Position", layer.props.prop("transform/position").unwrap().uid),
        ("Source Text", layer.props.prop("text/sourceText").unwrap().uid),
        ("the Properties group", layer.props.group("text/animators/#1/properties").unwrap().uid),
        ("Transform", layer.props.sub("transform").unwrap().uid),
    ];
    let before = s.project.clone();
    let undo = s.history.undo.len();
    for (what, u) in refused {
        let e = remove(&mut s, t, u).expect_err(what);
        assert!(e.to_string().contains("fixed"), "{what}: {e}");
    }
    assert!(remove(&mut s, t, 999_999_999).is_err(), "an unknown uid");
    assert_eq!(s.project, before, "nothing changed");
    assert_eq!(s.history.undo.len(), undo, "no undo step");
    // A selector is an instance: it can go.
    remove(&mut s, t, selector.uid).unwrap();
    assert_eq!(s.active_comp().unwrap().layer(LayerId(t)).unwrap().props.group("text/animators/#1/selectors").unwrap().groups().count(), 0);
}

#[test]
fn removing_deselects_the_property_and_its_keys() {
    let (mut s, t) = text_with_animator(&["blur", "opacity"]);
    s.execute("prop.addKey", json!({"layer": t, "path": "text/animators/#1/properties/blur", "time": 0.0, "value": [4, 4]})).unwrap();
    s.execute("prop.addKey", json!({"layer": t, "path": "text/animators/#1/properties/opacity", "time": 0.0, "value": 50})).unwrap();
    let (blur, opacity) = (uid(&s, t, "blur"), uid(&s, t, "opacity"));
    s.execute("prop.select", json!({"layer": t, "prop": blur})).unwrap();
    s.execute("prop.select", json!({"layer": t, "prop": opacity, "add": true})).unwrap();
    assert!(s.state.selected_keys.iter().any(|k| k.prop == blur));
    remove(&mut s, t, blur).unwrap();
    assert_eq!(s.state.selected_props, vec![(LayerId(t), opacity)], "only the remaining property stays selected");
    assert!(s.state.selected_keys.iter().all(|k: &KeyRef| k.prop == opacity), "the removed property's keys are deselected");
    assert!(!s.state.selected_keys.is_empty());
}

/// Edit ▸ Clear (the Delete key) with an animator property selected removes it (with its set),
/// not the layer, in one undo step.
#[test]
fn delete_with_an_animator_property_selected_removes_it() {
    let (mut s, t) = text_with_animator(&["fillColor", "tracking"]);
    let before = s.project.clone();
    let fill = uid(&s, t, "fillColor");
    s.execute("prop.select", json!({"layer": t, "prop": fill, "selectKeys": false})).unwrap();
    let undo = s.history.undo.len();
    s.execute("edit.clear", json!({})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    assert_eq!(kinds(&s, t), ["trackingType", "tracking"]);
    assert_eq!(s.active_comp().unwrap().layers.len(), 1, "the layer stays");
    assert!(s.state.selected_props.is_empty());
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);

    // Tracking Type selected: the pair goes. With Position selected too, only the pair goes.
    let ty = uid(&s, t, "trackingType");
    let pos = s.active_comp().unwrap().layer(LayerId(t)).unwrap().props.prop("transform/position").unwrap().uid;
    s.execute("prop.select", json!({"layer": t, "prop": ty, "selectKeys": false})).unwrap();
    s.execute("prop.select", json!({"layer": t, "prop": pos, "selectKeys": false, "add": true})).unwrap();
    s.execute("edit.clear", json!({})).unwrap();
    assert_eq!(kinds(&s, t), ["fillColor"]);
    assert_eq!(s.active_comp().unwrap().layers.len(), 1, "the layer stays");

    // The animator and one of its properties selected: the animator goes (once).
    let anim = s.active_comp().unwrap().layer(LayerId(t)).unwrap().props.group("text/animators").unwrap().groups().next().unwrap().uid;
    let fill = uid(&s, t, "fillColor");
    s.execute("prop.select", json!({"layer": t, "prop": anim, "selectKeys": false})).unwrap();
    s.execute("prop.select", json!({"layer": t, "prop": fill, "selectKeys": false, "add": true})).unwrap();
    let undo = s.history.undo.len();
    s.execute("edit.clear", json!({})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(s.active_comp().unwrap().layer(LayerId(t)).unwrap().props.group("text/animators").unwrap().groups().count(), 0);
    assert_eq!(s.active_comp().unwrap().layers.len(), 1);
}
