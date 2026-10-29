//! TEAM1: shadow compositions (`shadow.*`): create → work → checks → swap in as one undo step,
//! stale refusals when Elias edits the live comp, rebase diffs, deep copies of nested precomps,
//! claims for the badges, and junk input that never panics.

use effectcraft_project::{Expression, ItemId, LayerId, LayerSource, Project};
use serde_json::{Value, json};

use crate::Session;

struct Fx {
    s: Session,
    main: u64,
    scene: u64,
    card: u64,
    /// "Title" in the scene, "Card art" in Card.
    title: u64,
    art: u64,
}

/// MAIN › "S1 06" (Title, Card › Card art) and a Bg solid in MAIN.
fn setup() -> Fx {
    let mut s = Session::default();
    let main = s.execute("comp.new", json!({"name": "MAIN", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap()["comp"].as_u64().unwrap();
    let id = |s: &mut Session, name: &str| s.execute("layer.newSolid", json!({"name": name, "color": "#ff0000"})).unwrap()["layer"].as_u64().unwrap();
    let _bg = id(&mut s, "Bg");
    let title = id(&mut s, "Title");
    let art = id(&mut s, "Card art");
    let card = s.execute("layer.precompose", json!({"layers": [art], "name": "Card", "mode": "move"})).unwrap();
    let card_comp = card["comp"].as_u64().unwrap();
    let card_layer = card["layer"].as_u64().unwrap();
    let scene = s.execute("layer.precompose", json!({"layers": [title, card_layer], "name": "S1 06", "mode": "move"})).unwrap();
    let scene_comp = scene["comp"].as_u64().unwrap();
    let title = layer_named(&s, scene_comp, "Title");
    let art = layer_named(&s, card_comp, "Card art");
    Fx { s, main, scene: scene_comp, card: card_comp, title, art }
}

fn layer_named(s: &Session, comp: u64, name: &str) -> u64 {
    s.project.comp(ItemId(comp)).unwrap().layers.iter().find(|l| l.name == name).unwrap().id.0
}

fn precomps_of(s: &Session, comp: u64) -> Vec<u64> {
    s.project
        .comp(ItemId(comp))
        .map(|c| c.layers.iter().filter_map(|l| if let LayerSource::Comp { item } = &l.source { Some(item.0) } else { None }).collect())
        .unwrap_or_default()
}

fn opacity(s: &Session, comp: u64, layer: u64) -> f64 {
    s.project.comp(ItemId(comp)).unwrap().layer(LayerId(layer)).unwrap().props.prop("transform/opacity").unwrap().value.as_f64()
}

fn set_opacity(s: &mut Session, comp: u64, layer: u64, v: f64, client: Option<&str>) -> crate::Result<Value> {
    let mut p = json!({"comp": comp, "layer": layer, "path": "transform/opacity", "value": v});
    if let Some(c) = client {
        p["client"] = json!(c);
    }
    s.execute_checked("prop.set", p)
}

fn name_of(s: &Session, id: u64) -> String {
    s.project.item(ItemId(id)).map(|i| i.name.clone()).unwrap_or_default()
}

fn create(s: &mut Session, comp: u64, agent: &str) -> (u64, Value) {
    let r = s.execute_checked("shadow.create", json!({"comp": comp, "agent": agent})).unwrap();
    (r["shadow"].as_u64().unwrap(), r)
}

/// The shadow's copy of live comp `live`.
fn copy_of(r: &Value, live: u64) -> u64 {
    r["copies"].as_array().unwrap().iter().find(|c| c["live"].as_u64() == Some(live)).unwrap()["copy"].as_u64().unwrap()
}

fn pass(s: &mut Session, shadow: u64) {
    s.execute_checked(
        "shadow.setStatus",
        json!({"shadow": shadow, "status": "passed", "checks": [{"id": "lint", "ok": true}, {"id": "taste", "ok": true, "note": "eases ok"}]}),
    )
    .unwrap();
}

#[test]
fn create_swap_undo_round_trip() {
    let mut f = setup();
    let main_uses = precomps_of(&f.s, f.main);
    assert_eq!(main_uses, [f.scene]);
    let (shadow, r) = create(&mut f.s, f.scene, "A2");
    assert_eq!(name_of(&f.s, shadow), "S1 06 ✎A2");
    assert_eq!(r["status"], "working");
    assert_eq!(r["stale"], false);
    let card_copy = copy_of(&r, f.card);
    assert_eq!(name_of(&f.s, card_copy), "Card ✎A2");
    // In the hidden Shadows folder; the copies use each other; the live comps are untouched.
    let folder = r["folder"].as_u64().unwrap();
    assert_eq!(name_of(&f.s, folder), "Shadows");
    assert!([shadow, card_copy].iter().all(|c| f.s.project.item(ItemId(*c)).unwrap().parent == Some(ItemId(folder))));
    assert_eq!(precomps_of(&f.s, shadow), [card_copy]);
    assert_eq!(precomps_of(&f.s, f.scene), [f.card]);
    // Footage / solids are shared, not copied.
    let solid_src = |s: &Session, comp: u64| {
        s.project.comp(ItemId(comp)).unwrap().layers.iter().find_map(|l| if let LayerSource::Solid { item } = &l.source { Some(*item) } else { None })
    };
    assert_eq!(solid_src(&f.s, card_copy), solid_src(&f.s, f.card));

    // The agent works in the shadow (scene layer and nested card layer).
    let s_title = layer_named(&f.s, shadow, "Title");
    let s_art = layer_named(&f.s, card_copy, "Card art");
    set_opacity(&mut f.s, shadow, s_title, 40.0, Some("A2")).unwrap();
    set_opacity(&mut f.s, card_copy, s_art, 25.0, Some("A2")).unwrap();
    assert_eq!(opacity(&f.s, f.scene, f.title), 100.0, "the live comp is untouched");
    pass(&mut f.s, shadow);
    let undo_len = f.s.history.undo.len();

    let r = f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow})).unwrap();
    assert_eq!(r["status"], "swapped", "{r}");
    assert_eq!(f.s.history.undo.len(), undo_len + 1, "one undo step");
    // The live comp keeps its id, name and use in MAIN, and has the shadow's contents.
    assert_eq!(precomps_of(&f.s, f.main), main_uses);
    assert_eq!(name_of(&f.s, f.scene), "S1 06");
    assert_eq!(name_of(&f.s, f.card), "Card");
    let live_title = layer_named(&f.s, f.scene, "Title");
    assert_eq!(opacity(&f.s, f.scene, live_title), 40.0);
    assert_eq!(precomps_of(&f.s, f.scene), [f.card], "the nested precomp is the live one, not the copy");
    let live_art = layer_named(&f.s, f.card, "Card art");
    assert_eq!(opacity(&f.s, f.card, live_art), 25.0);
    // The shadow holds Before.
    assert_eq!(opacity(&f.s, shadow, f.title), 100.0);
    assert_eq!(precomps_of(&f.s, shadow), [card_copy]);
    assert_eq!(r["before"], json!(shadow));
    assert_eq!(r["after"], json!(f.scene));

    // Undo restores the live comp; redo swaps again.
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(opacity(&f.s, f.scene, f.title), 100.0);
    assert_eq!(opacity(&f.s, f.card, f.art), 100.0);
    let g = f.s.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap();
    assert_eq!(g["status"], "passed", "{g}");
    f.s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(f.s.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap()["status"], "swapped");

    // shadow.revert {comp} swaps Before back in.
    let r = f.s.execute_checked("shadow.revert", json!({"comp": f.scene})).unwrap();
    assert_eq!(r["status"], "passed", "{r}");
    assert_eq!(opacity(&f.s, f.scene, f.title), 100.0);
    assert_eq!(precomps_of(&f.s, f.main), main_uses);
    assert_eq!(precomps_of(&f.s, f.scene), [f.card]);

    let kinds: Vec<String> = f.s.execute_checked("shadow.events", json!({})).unwrap()["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.starts_with(&["created".into(), "status".into(), "swapped".into()]), "{kinds:?}");
    assert!(kinds.contains(&"reverted".to_string()));
}

#[test]
fn a_stale_shadow_refuses_to_swap_without_force() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "A2");
    let seq = f.s.execute_checked("shadow.events", json!({})).unwrap()["next"].as_u64().unwrap();
    pass(&mut f.s, shadow);
    // Elias edits the live comp: allowed (he always wins), the shadow is stale.
    set_opacity(&mut f.s, f.scene, f.title, 60.0, None).unwrap();
    assert_eq!(opacity(&f.s, f.scene, f.title), 60.0);
    let g = f.s.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap();
    assert_eq!(g["stale"], true, "{g}");
    assert!(g["changes"]["summary"].as_str().unwrap().contains("Title"), "{g}");
    let ev = f.s.execute_checked("shadow.events", json!({"since": seq})).unwrap();
    let stale: Vec<&Value> = ev["events"].as_array().unwrap().iter().filter(|e| e["kind"] == "stale").collect();
    assert_eq!(stale.len(), 1, "{ev}");
    assert_eq!(stale[0]["shadow"], json!(shadow));
    // Polling again logs nothing new.
    let next = ev["next"].as_u64().unwrap();
    assert_eq!(f.s.execute_checked("shadow.events", json!({"since": next})).unwrap()["events"], json!([]));

    let (rev, project) = (f.s.revision, f.s.project.clone());
    let e = f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow})).unwrap_err().to_string();
    assert!(e.contains("stale") && e.contains("force"), "{e}");
    assert!(std::sync::Arc::ptr_eq(&f.s.project, &project) && f.s.revision == rev, "nothing changed");
    let r = f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow, "force": true})).unwrap();
    assert_eq!(r["status"], "swapped");
    assert!(!r["forced"].as_array().unwrap().is_empty());
    // A shadow that didn't pass needs force too.
    let (other, _) = create(&mut f.s, f.card, "A3");
    let e = f.s.execute_checked("shadow.swapIn", json!({"shadow": other})).unwrap_err().to_string();
    assert!(e.contains("not passed"), "{e}");
}

#[test]
fn rebase_returns_what_elias_changed_and_copies_his_version() {
    let mut f = setup();
    let (shadow, r) = create(&mut f.s, f.scene, "A2");
    let card_copy = copy_of(&r, f.card);
    let s_title = layer_named(&f.s, shadow, "Title");
    set_opacity(&mut f.s, shadow, s_title, 10.0, Some("A2")).unwrap();
    // Elias: changes Title's opacity and adds a layer to the scene.
    set_opacity(&mut f.s, f.scene, f.title, 55.0, None).unwrap();
    f.s.execute("comp.open", json!({"comp": f.scene})).unwrap();
    f.s.execute("layer.newSolid", json!({"comp": f.scene, "name": "Flash", "color": "#ffffff"})).unwrap();

    let r = f.s.execute_checked("shadow.rebase", json!({"shadow": shadow})).unwrap();
    assert_eq!(r["shadow"], json!(shadow), "same shadow id");
    assert_eq!(r["status"], "working");
    assert_eq!(r["stale"], false, "{r}");
    let d = &r["elias"];
    assert_eq!(d["changed"], true, "{d}");
    let comp = d["comps"].as_array().unwrap().iter().find(|c| c["comp"].as_u64() == Some(f.scene)).unwrap();
    let changed = comp["changed"].as_array().unwrap();
    let title = changed.iter().find(|c| c["name"] == "Title").unwrap();
    assert!(title["props"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().contains("Opacity")), "{title}");
    assert!(comp["added"].as_array().unwrap().iter().any(|l| l["name"] == "Flash"), "{comp}");
    // The shadow is his version now (the agent redoes its work on top).
    assert_eq!(opacity(&f.s, shadow, layer_named(&f.s, shadow, "Title")), 55.0);
    assert!(f.s.project.comp(ItemId(shadow)).unwrap().layers.iter().any(|l| l.name == "Flash"));
    assert_eq!(precomps_of(&f.s, shadow), [card_copy], "the nested copy is reused");
}

#[test]
fn precomps_are_copied_deeply_with_their_expression_references() {
    let mut f = setup();
    // One more level: Card › Inner › Card art.
    f.s.execute("comp.open", json!({"comp": f.card})).unwrap();
    let inner = f.s.execute("layer.precompose", json!({"comp": f.card, "layers": [f.art], "name": "Inner", "mode": "move"})).unwrap()["comp"].as_u64().unwrap();
    // An expression in the scene that names the Card comp.
    {
        let p = std::sync::Arc::make_mut(&mut f.s.project);
        let l = p.comp_mut(ItemId(f.scene)).unwrap().layer_mut(LayerId(f.title)).unwrap();
        l.props.prop_mut("transform/opacity").unwrap().expr = Some(Expression { text: "comp(\"Card\").duration * 10".into(), enabled: true });
    }
    let (shadow, r) = create(&mut f.s, f.scene, "A2");
    assert_eq!(r["copies"].as_array().unwrap().len(), 3, "{r}");
    let (card_copy, inner_copy) = (copy_of(&r, f.card), copy_of(&r, inner));
    assert_eq!(name_of(&f.s, inner_copy), "Inner ✎A2");
    assert_eq!(precomps_of(&f.s, shadow), [card_copy]);
    assert_eq!(precomps_of(&f.s, card_copy), [inner_copy]);
    assert_eq!(precomps_of(&f.s, f.card), [inner], "the originals are untouched");
    let expr = |s: &Session, comp: u64| {
        s.project.comp(ItemId(comp)).unwrap().layer_by_name("Title").unwrap().props.prop("transform/opacity").unwrap().expr.clone().unwrap().text
    };
    assert_eq!(expr(&f.s, shadow), "comp(\"Card ✎A2\").duration * 10");
    // Fresh layer ids in the copies.
    let ids = |s: &Session, c: u64| s.project.comp(ItemId(c)).unwrap().layers.iter().map(|l| l.id).collect::<Vec<_>>();
    assert!(ids(&f.s, shadow).iter().all(|i| !ids(&f.s, f.scene).contains(i)));
    // After the swap the live scene names the live Card again, and uses the live chain.
    pass(&mut f.s, shadow);
    f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow})).unwrap();
    assert_eq!(expr(&f.s, f.scene), "comp(\"Card\").duration * 10");
    assert_eq!(precomps_of(&f.s, f.scene), [f.card]);
    assert_eq!(precomps_of(&f.s, f.card), [inner]);
    assert_eq!(precomps_of(&f.s, card_copy), [inner_copy]);
    // Drop deletes the shadow and its copies (the folder goes with the last one).
    let d = f.s.execute_checked("shadow.drop", json!({"shadow": shadow})).unwrap();
    assert_eq!(d["status"], "dropped");
    assert!(f.s.project.item(ItemId(shadow)).is_none() && f.s.project.item(ItemId(inner_copy)).is_none());
    assert!(f.s.project.folder_named("Shadows").is_none());
    assert_eq!(precomps_of(&f.s, f.scene), [f.card], "the live comps stay");
    assert_eq!(f.s.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap()["status"], "dropped");
}

#[test]
fn a_working_shadow_claims_its_source_but_elias_still_edits_it() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "A2");
    let c = f.s.execute_checked("comp.claims", json!({})).unwrap();
    let claim = c["claims"].as_array().unwrap().iter().find(|x| x["comp"].as_u64() == Some(f.scene)).cloned().unwrap();
    assert_eq!(claim["client"], "A2");
    assert_eq!(c["shadows"][0]["shadow"], json!(shadow));
    // Another agent can't edit the live comp; Elias can, and it marks the shadow stale.
    assert!(set_opacity(&mut f.s, f.scene, f.title, 5.0, Some("B1")).is_err());
    set_opacity(&mut f.s, f.scene, f.title, 70.0, None).unwrap();
    assert_eq!(f.s.execute_checked("shadow.list", json!({"comp": f.scene})).unwrap()["shadows"][0]["stale"], true);
    // Passed: no longer worked on, the claim goes.
    pass(&mut f.s, shadow);
    let c = f.s.execute_checked("comp.claims", json!({})).unwrap();
    assert!(c["claims"].as_array().unwrap().iter().all(|x| x["comp"].as_u64() != Some(f.scene)), "{c}");
    // A second shadow of the same comp by the same agent is refused; a shadow of a shadow too.
    assert!(f.s.execute_checked("shadow.create", json!({"comp": f.scene, "agent": "A2"})).is_err());
    assert!(f.s.execute_checked("shadow.create", json!({"comp": shadow, "agent": "A9"})).is_err());
    // By agent: the list filters.
    assert_eq!(f.s.execute_checked("shadow.list", json!({"agent": "nobody"})).unwrap()["shadows"], json!([]));
}

fn claim_owner(s: &mut Session, comp: u64) -> Option<String> {
    let c = s.execute_checked("comp.claims", json!({})).unwrap();
    c["claims"].as_array().unwrap().iter().find(|x| x["comp"].as_u64() == Some(comp)).and_then(|x| x["client"].as_str().map(str::to_string))
}

/// 1028-137: a dropped shadow's 24 h claim went with it only while the agent had another shadow.
#[test]
fn dropping_an_agents_last_shadow_releases_its_claim() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "editing-chat-4");
    assert_eq!(claim_owner(&mut f.s, f.scene).as_deref(), Some("editing-chat-4"));
    f.s.execute_checked("shadow.drop", json!({"shadow": shadow})).unwrap();
    assert_eq!(claim_owner(&mut f.s, f.scene), None, "the claim goes with the agent's last shadow");
    assert!(set_opacity(&mut f.s, f.scene, f.title, 5.0, Some("editing-team")).is_ok());
}

/// 1028-137: a stopped team run releases its shadows: kept, parked, the source unclaimed; working
/// on one again takes the claim back.
#[test]
fn release_keeps_the_shadow_and_lifts_its_claim_until_work_resumes() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "editing-chat-4");
    let (other, _) = create(&mut f.s, f.card, "editing-chat-4");
    let r = f.s.execute_checked("shadow.release", json!({"agent": "editing-chat-4"})).unwrap();
    assert_eq!(r["released"].as_array().unwrap().len(), 2, "{r}");
    assert_eq!(r["shadows"][0]["parked"], true);
    assert_eq!(claim_owner(&mut f.s, f.scene), None);
    assert_eq!(claim_owner(&mut f.s, f.card), None);
    assert!(f.s.project.comp(ItemId(shadow)).is_some() && f.s.project.comp(ItemId(other)).is_some(), "the shadows are kept");
    // Another client may now edit the source; refreshes don't take the claim back.
    set_opacity(&mut f.s, f.scene, f.title, 5.0, Some("editing-team")).unwrap();
    f.s.execute_checked("shadow.list", json!({})).unwrap();
    assert_eq!(claim_owner(&mut f.s, f.scene), None);
    // Parked survives a save and reopen.
    let text = f.s.project.to_file_json().unwrap();
    let mut s2 = Session::default();
    s2.replace_project(Project::from_json(&text).unwrap(), None);
    assert_eq!(s2.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap()["parked"], true);
    assert_eq!(claim_owner(&mut s2, f.scene), None);
    // Work resumes: the claim is back.
    f.s.execute_checked("shadow.setStatus", json!({"shadow": shadow, "status": "working"})).unwrap();
    assert_eq!(claim_owner(&mut f.s, f.scene).as_deref(), Some("editing-chat-4"));
    assert_eq!(f.s.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap()["parked"], false);
    assert!(f.s.execute_checked("shadow.release", json!({})).is_err(), "release needs a target");
}

/// 1028-137: `force` swaps in over a stale claim another client holds; without it the refusal
/// says so (Ghostex offers "Keep anyway" when the error names `force`).
#[test]
fn force_swaps_in_over_another_clients_claim() {
    let mut f = setup();
    f.s.execute_checked("comp.claim", json!({"comp": f.scene, "ttl": 86_400, "client": "editing-chat-4"})).unwrap();
    let (shadow, _) = create(&mut f.s, f.scene, "editing-chat-5");
    let s_title = layer_named(&f.s, shadow, "Title");
    set_opacity(&mut f.s, shadow, s_title, 40.0, Some("editing-chat-5")).unwrap();
    pass(&mut f.s, shadow);
    let e = f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow, "client": "editing-team"})).unwrap_err().to_string();
    assert!(e.contains("claimed by editing-chat-4") && e.contains("force"), "{e}");
    assert_eq!(claim_owner(&mut f.s, f.scene).as_deref(), Some("editing-chat-4"), "a refusal leaves the claim");
    let r = f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow, "client": "editing-team", "force": true})).unwrap();
    assert_eq!(r["status"], "swapped", "{r}");
    assert_eq!(opacity(&f.s, f.scene, layer_named(&f.s, f.scene, "Title")), 40.0);
    // The user (no client) is never blocked by a claim.
    f.s.execute_checked("comp.claim", json!({"comp": f.card, "ttl": 86_400, "client": "B1"})).unwrap();
    let (shadow2, _) = create(&mut f.s, f.card, "editing-chat-5");
    pass(&mut f.s, shadow2);
    assert_eq!(f.s.execute_checked("shadow.swapIn", json!({"shadow": shadow2})).unwrap()["status"], "swapped");
}

#[test]
fn shadows_and_their_status_survive_a_save_and_reopen() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "A2");
    f.s.execute_checked("shadow.setStatus", json!({"shadow": shadow, "status": "failed", "checks": [{"id": "lint", "ok": false, "note": "text too small"}]}))
        .unwrap();
    let text = f.s.project.to_file_json().unwrap();
    let mut s2 = Session::default();
    s2.replace_project(Project::from_json(&text).unwrap(), None);
    let l = s2.execute_checked("shadow.list", json!({})).unwrap();
    let x = &l["shadows"][0];
    assert_eq!(x["shadow"], json!(shadow));
    assert_eq!(x["status"], "failed");
    assert_eq!(x["checks"][0]["note"], "text too small");
    assert_eq!(x["stale"], false, "the fingerprints still match after the reopen: {x}");
    assert_eq!(x["agent"], "A2");
}

#[test]
fn a_status_survives_undo_of_an_earlier_step() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "A2");
    let bg = layer_named(&f.s, f.main, "Bg");
    set_opacity(&mut f.s, f.main, bg, 50.0, None).unwrap();
    pass(&mut f.s, shadow);
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(f.s.execute_checked("shadow.get", json!({"shadow": shadow})).unwrap()["status"], "passed");
    assert_eq!(f.s.project.shadows[0].status, effectcraft_project::shadow::ShadowStatus::Passed, "written back into the project");
}

#[test]
fn frames_render_before_and_after_with_audio_info() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "A2");
    for which in ["before", "after"] {
        let r = f.s.execute_checked("shadow.frames", json!({"shadow": shadow, "which": which, "times": [0.0, 1.0], "max_side": 64, "base64": true})).unwrap();
        assert_eq!(r["frames"].as_array().unwrap().len(), 2, "{r}");
        assert!(r["frames"][0]["png"].as_str().is_some_and(|p| !p.is_empty()));
        assert_eq!(r["comp"], json!(if which == "before" { f.scene } else { shadow }));
        assert_eq!(r["audio"]["hasAudio"], false);
    }
    let r = f.s.execute_checked("shadow.frames", json!({"comp": f.scene, "max_side": 32, "base64": true})).unwrap();
    assert_eq!(r["which"], "after");
    assert!(!r["frames"].as_array().unwrap().is_empty());
}

#[test]
fn junk_input_never_panics() {
    let mut f = setup();
    let (shadow, _) = create(&mut f.s, f.scene, "A2");
    let caught = crate::guard::caught();
    let junk = [
        json!(null),
        json!([]),
        json!("x"),
        json!({}),
        json!({"shadow": null}),
        json!({"shadow": -1}),
        json!({"shadow": 1.5}),
        json!({"shadow": u64::MAX}),
        json!({"shadow": "nope"}),
        json!({"shadow": [shadow]}),
        json!({"shadow": {"id": shadow}}),
        json!({"shadow": shadow, "status": 7}),
        json!({"shadow": shadow, "status": "swapped"}),
        json!({"shadow": shadow, "status": "passed", "checks": "all"}),
        json!({"shadow": shadow, "status": "passed", "checks": [1, null, {"id": 3}, {"id": "", "ok": true}, {"id": "x", "ok": "yes"}]}),
        json!({"shadow": shadow, "status": "passed", "checks": [{"id": "x".repeat(10_000), "ok": true}]}),
        json!({"shadow": shadow, "which": "middle"}),
        json!({"shadow": shadow, "times": [f64::MAX, -1e300, "a"]}),
        json!({"shadow": shadow, "times": (0..100).collect::<Vec<_>>()}),
        json!({"shadow": shadow, "max_side": 0}),
        json!({"shadow": shadow, "force": "yes"}),
        json!({"comp": -3}),
        json!({"comp": "no such comp", "agent": "A"}),
        json!({"comp": f.scene, "agent": ""}),
        json!({"comp": f.scene, "agent": "\u{0}\u{7}"}),
        json!({"comp": f.scene, "agent": "a".repeat(1000)}),
        json!({"comp": f.scene, "agent": 12}),
        json!({"since": -5}),
        json!({"since": "x", "limit": 0}),
        json!({"since": u64::MAX, "limit": u64::MAX}),
    ];
    let ids = [
        "shadow.create",
        "shadow.list",
        "shadow.get",
        "shadow.setStatus",
        "shadow.swapIn",
        "shadow.revert",
        "shadow.rebase",
        "shadow.frames",
        "shadow.events",
        "shadow.release",
        "shadow.drop",
    ];
    for id in ids {
        for p in &junk {
            let _ = f.s.execute(id, p.clone());
        }
    }
    assert_eq!(crate::guard::caught(), caught, "no command panicked");
    // A link pointing at comps that are gone (a hand-edited file) is reported, not a crash.
    {
        let p = std::sync::Arc::make_mut(&mut f.s.project);
        p.shadows.push(effectcraft_project::shadow::ShadowLink {
            shadow: ItemId(999_999),
            source: ItemId(888_888),
            agent: "ghost".into(),
            ..Default::default()
        });
    }
    let l = f.s.execute_checked("shadow.list", json!({})).unwrap();
    assert!(l["shadows"].as_array().unwrap().iter().any(|x| x["broken"] == true), "{l}");
    for id in ids {
        let _ = f.s.execute(id, json!({"shadow": 999_999}));
    }
    assert_eq!(crate::guard::caught(), caught);
}
