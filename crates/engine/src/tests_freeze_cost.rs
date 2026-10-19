//! Heavy-layer suggestions for Smart Freeze: synthetic render times, no rendering needed.

use effectcraft_project::{ItemId, LayerId};
use serde_json::json;

use crate::{Session, freeze_cost::Unit};

struct Fixture {
    s: Session,
    outer: ItemId,
    inner: ItemId,
    stack: LayerId,
    solid: LayerId,
    inside: LayerId,
}

/// "Outer" (10 fps, so a 100 ms budget) holds a cheap solid and the precomp layer "Stat Stack"
/// of "Inner", which holds one layer of its own.
fn fixture() -> Fixture {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Inner", "width":16, "height":16, "frameRate":10, "duration":0.2})).unwrap();
    let inner = s.active_comp_id().unwrap();
    s.execute("layer.newSolid", json!({"name":"Inside", "color":"#00ff00", "width":8, "height":8})).unwrap();
    let inside = s.active_comp().unwrap().layers[0].id;
    s.execute("comp.new", json!({"name":"Outer", "width":16, "height":16, "frameRate":10, "duration":0.2})).unwrap();
    let outer = s.active_comp_id().unwrap();
    s.execute("layer.newSolid", json!({"name":"Red", "color":"#ff0000", "width":8, "height":8})).unwrap();
    let solid = s.active_comp().unwrap().layers[0].id;
    s.execute("layer.addItem", json!({"item":inner.0})).unwrap();
    let stack = s.active_comp().unwrap().layers[0].id;
    s.execute("layer.rename", json!({"layer":stack.0, "name":"Stat Stack"})).ok();
    Fixture { s, outer, inner, stack, solid, inside }
}

fn play(f: &mut Fixture, frames: usize, stack_ms: f64, solid_ms: f64, inside_ms: f64) {
    for _ in 0..frames {
        f.s.note_layer_costs(f.outer, [(f.outer, f.stack, stack_ms), (f.outer, f.solid, solid_ms), (f.inner, f.inside, inside_ms)]);
    }
}

#[test]
fn a_heavy_precomp_is_suggested_and_a_cheap_layer_is_not() {
    let mut f = fixture();
    play(&mut f, 12, 1800.0, 2.0, 1700.0);
    let found = f.s.freeze_suggestions();
    assert_eq!(found.len(), 1, "the solid is cheap and the layer inside is covered by freezing its precomp: {found:?}");
    let top = &found[0];
    assert_eq!((top.comp, top.layer, top.unit), (f.outer, f.stack, Unit::Precomp(f.inner)));
    assert!((top.ratio() - 18.0).abs() < 1e-6, "1800 ms against a 100 ms budget");
    assert_eq!(top.question(), "Stat Stack renders at 1.8 s/frame — Freeze it?");
    let listed = f.s.execute("freeze.suggest", json!({})).unwrap();
    assert_eq!(listed["candidates"][0]["layer"], f.stack.0);
    assert_eq!(listed["candidates"][0]["freeze"]["mode"], "precomp");
    assert_eq!(listed["autoHeavy"], false);
    assert!(f.s.project.freeze.sections.is_empty(), "suggesting never freezes");
}

#[test]
fn only_a_whole_section_of_slow_frames_counts() {
    let mut f = fixture();
    play(&mut f, 7, 1800.0, 2.0, 2.0);
    assert!(f.s.freeze_suggestions().is_empty(), "too few frames measured");
    play(&mut f, 1, 1800.0, 2.0, 2.0);
    assert_eq!(f.s.freeze_suggestions().len(), 1);
    play(&mut f, 16, 40.0, 2.0, 2.0);
    assert!(f.s.freeze_suggestions().is_empty(), "back under the budget once the section has passed");
    let mut g = fixture();
    for i in 0..20 {
        play(&mut g, 1, if i % 5 == 0 { 1800.0 } else { 30.0 }, 2.0, 2.0);
    }
    assert!(g.s.freeze_suggestions().is_empty(), "occasional slow frames are not a heavy section");
}

#[test]
fn a_heavy_layer_inside_the_comp_is_suggested_and_bad_times_are_ignored() {
    let mut f = fixture();
    play(&mut f, 12, 5.0, 900.0, 3.0);
    f.s.note_layer_costs(f.outer, [(f.outer, f.solid, f64::NAN), (f.outer, f.solid, -4.0), (f.outer, LayerId(9999), 900.0)]);
    f.s.note_layer_costs(ItemId(9999), [(f.outer, f.solid, 900.0)]);
    let found = f.s.freeze_suggestions();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!((found[0].layer, found[0].unit), (f.solid, Unit::Layer));
    assert_eq!(found[0].freeze_params()["mode"], "selection");
}

#[test]
fn dismissing_hides_a_suggestion_and_freezing_it_clears_it_with_undo() {
    let mut f = fixture();
    play(&mut f, 12, 1800.0, 2.0, 2.0);
    f.s.execute("freeze.suggest", json!({"dismiss":{"comp":f.outer.0, "layer":f.stack.0}})).unwrap();
    assert!(f.s.freeze_suggestions().is_empty());

    let mut f = fixture();
    play(&mut f, 12, 1800.0, 2.0, 2.0);
    let mut params = f.s.freeze_suggestions()[0].freeze_params();
    params["wait"] = json!(true);
    params["quickDraft"] = json!(false);
    f.s.execute("freeze.freeze", params).unwrap();
    let section = &f.s.project.freeze.sections[0];
    assert!(section.auto && section.cache.is_some(), "the suggestion's freeze keeps itself fresh (Auto)");
    assert!(f.s.freeze_suggestions().is_empty(), "a frozen layer is not suggested again");
    assert!(f.s.undo(), "freezing is an undo step");
    assert!(f.s.project.freeze.sections.is_empty() || f.s.project.freeze.sections[0].cache.is_none());
    std::fs::remove_dir_all(crate::freeze_storage::root(&f.s.project.freeze.namespace).unwrap()).ok();
}

#[test]
fn heavy_layers_freeze_by_themselves_only_with_the_preference_and_when_idle() {
    let mut f = fixture();
    play(&mut f, 12, 1800.0, 2.0, 2.0);
    idle_for(&mut f.s, 30);
    f.s.poll_freeze();
    f.s.wait_jobs();
    assert!(f.s.project.freeze.sections.is_empty(), "preference off: nothing freezes without a click");

    f.s.prefs.previews.freeze_auto_heavy = true;
    idle_for(&mut f.s, 2);
    f.s.poll_freeze();
    assert!(f.s.project.freeze.sections.is_empty(), "not idle yet");

    idle_for(&mut f.s, 30);
    f.s.poll_freeze();
    f.s.wait_jobs();
    assert_eq!(f.s.project.freeze.sections.len(), 1);
    assert!(f.s.project.freeze.sections[0].auto);
    assert!(f.s.project.freeze.sections[0].cache.is_some());
    std::fs::remove_dir_all(crate::freeze_storage::root(&f.s.project.freeze.namespace).unwrap()).ok();
}

fn idle_for(s: &mut Session, seconds: u64) {
    let ago = web_time::Instant::now() - std::time::Duration::from_secs(seconds);
    s.freeze_runtime.changed = ago;
    s.freeze_runtime.activity = ago;
}

#[test]
fn hints_belong_to_the_open_composition() {
    let mut f = fixture();
    // Only the layer inside "Inner" is slow, and "Outer" is open: nothing to say about it.
    play(&mut f, 12, 5.0, 2.0, 400.0);
    assert!(f.s.freeze_suggestions().is_empty(), "a layer of another comp is not this comp's hint");
    assert_eq!(f.s.execute("freeze.suggest", json!({})).unwrap()["candidates"].as_array().unwrap().len(), 0);
    let all = f.s.execute("freeze.suggest", json!({"all":true})).unwrap();
    assert_eq!(all["candidates"][0]["layer"], f.inside.0, "a caller can still ask for every comp");
    let inner = f.s.execute("freeze.suggest", json!({"comp":f.inner.0})).unwrap();
    assert_eq!(inner["candidates"][0]["layer"], f.inside.0);

    f.s.execute("comp.open", json!({"comp":f.inner.0})).unwrap();
    let found = f.s.freeze_suggestions();
    assert_eq!((found.len(), found[0].layer), (1, f.inside), "opening Inner shows Inner's hint");
    f.s.execute("comp.open", json!({"comp":f.outer.0})).unwrap();
    assert!(f.s.freeze_suggestions().is_empty(), "and opening Outer takes it away again");
}

#[test]
fn a_layer_freeze_refuses_is_offered_as_its_whole_comp_or_not_at_all() {
    let mut f = fixture();
    f.s.execute("prop.setExpression", json!({"comp":f.outer.0, "layer":f.solid.0, "path":"transform/position", "expression":"[value[0], value[1]]"})).unwrap();
    play(&mut f, 12, 5.0, 900.0, 2.0);
    let refused = f.s.execute("freeze.freeze", json!({"comp":f.outer.0, "layers":[f.solid.0], "mode":"selection"})).unwrap_err().to_string();
    assert!(refused.contains("Red has expressions"), "{refused}");

    let found = f.s.freeze_suggestions();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!((found[0].layer, found[0].unit), (f.solid, Unit::Comp));
    assert_eq!(found[0].question(), "Freeze Outer (Red renders at 900 ms/frame)");
    let params = found[0].freeze_params();
    assert_eq!(params["mode"], "comp");

    let mut run = params;
    run["wait"] = json!(true);
    run["quickDraft"] = json!(false);
    f.s.execute("freeze.freeze", run).expect("what the hint offers is what freeze.freeze accepts");
    assert!(f.s.project.freeze.sections[0].cache.is_some());
    assert!(f.s.freeze_suggestions().is_empty(), "and once frozen it is not offered again");
    std::fs::remove_dir_all(crate::freeze_storage::root(&f.s.project.freeze.namespace).unwrap()).ok();

    // The comp itself cannot be frozen (it sits collapsed in another comp): no hint at all.
    let mut g = fixture();
    g.s.execute("prop.setExpression", json!({"comp":g.outer.0, "layer":g.solid.0, "path":"transform/position", "expression":"[value[0], value[1]]"})).unwrap();
    g.s.execute("comp.new", json!({"name":"Top", "width":16, "height":16, "frameRate":10, "duration":0.2})).unwrap();
    let top = g.s.active_comp_id().unwrap();
    g.s.execute("layer.addItem", json!({"item":g.outer.0})).unwrap();
    let nested = g.s.active_comp().unwrap().layers[0].id;
    g.s.execute("layer.setSwitch", json!({"comp":top.0, "layers":[nested.0], "switch":"collapse", "value":true})).unwrap();
    g.s.execute("comp.open", json!({"comp":g.outer.0})).unwrap();
    play(&mut g, 12, 5.0, 900.0, 2.0);
    assert!(g.s.freeze_suggestions().is_empty(), "neither the layer nor its comp can be frozen: {:?}", g.s.freeze_suggestions());
}

#[test]
fn expressions_on_other_layers_do_not_block_a_selection_freeze() {
    let mut f = fixture();
    // The precomp layer carries an expression; the selected solid has none.
    f.s.execute("prop.setExpression", json!({"comp":f.outer.0, "layer":f.stack.0, "path":"transform/position", "expression":"[value[0], value[1]]"})).unwrap();
    let run = json!({"comp":f.outer.0, "layers":[f.solid.0], "mode":"selection", "wait":true, "quickDraft":false});
    f.s.execute("freeze.freeze", run).expect("an unrelated expression leaves the selection self-contained");
    assert!(f.s.project.freeze.sections[0].cache.is_some());
}

#[test]
fn a_selection_parented_outside_names_the_parent() {
    let mut f = fixture();
    f.s.execute("layer.setParent", json!({"layers":[f.solid.0], "parent":f.stack.0})).unwrap();
    let refused = f.s.execute("freeze.freeze", json!({"comp":f.outer.0, "layers":[f.solid.0], "mode":"selection"})).unwrap_err().to_string();
    assert!(refused.contains("Red is parented to"), "{refused}");
    assert!(refused.contains("which is not selected"), "{refused}");
}
