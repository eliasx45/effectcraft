//! Opening a project (G5): effect upgrades only copy and rewrite the layers that need them, and an
//! opened project is the same as before the speed-up.

use std::sync::Arc;
use std::time::Instant;

use effectcraft_project::build::{self, Ids};
use effectcraft_project::{GroupKind, ItemId, ItemKind, LayerSource, Node, ParamUi, Project, PropGroup};
use serde_json::json;

use crate::{Session, upgrade_effects};

const EFFECTS: [&str; 3] = ["ec.blur.gaussian", "ec.color.selectivecolor", "ec.transition.cardwipe"];

/// A comp of `n` null layers with three effects each.
fn large(n: usize) -> Project {
    let mut s = Session::default();
    let r = s.execute("comp.new", json!({"name": "Big", "width": 640, "height": 360, "duration": 2})).unwrap();
    let cid = ItemId(r["comp"].as_u64().unwrap());
    let mut p = (*s.project).clone();
    let comp = p.comp(cid).unwrap().clone();
    let mut layers = vec![];
    for i in 0..n {
        let mut l = build::layer(&mut p, &comp, &format!("L{i}"), LayerSource::Null, (640, 360), None);
        for id in EFFECTS {
            let spec = crate::effects::find(id).unwrap();
            let mut next = p.next_id;
            let g = crate::effects::instantiate(spec, &mut Ids(&mut next), spec.name, [640.0, 360.0]);
            p.next_id = next;
            l.props.sub_mut("effects").unwrap().children.push(g.into());
        }
        layers.push(l);
    }
    p.comp_mut(cid).unwrap().layers.extend(layers);
    p
}

fn effect_groups(p: &mut Project) -> Vec<&mut PropGroup> {
    let mut out = vec![];
    for it in p.items.values_mut() {
        let ItemKind::Comp(c) = &mut it.kind else { continue };
        for l in &mut Arc::make_mut(c).layers {
            let Some(fx) = l.props.sub_mut("effects") else { continue };
            for n in &mut fx.children {
                if let Node::Group(g) = n
                    && matches!(g.kind, GroupKind::Effect { .. })
                {
                    out.push(g);
                }
            }
        }
    }
    out
}

/// `p` as saved by an earlier version: before effect space, each instance missing its last
/// parameter, with a stale name and a reordered popup.
fn stale(mut p: Project) -> Project {
    p.schema = 1;
    for g in effect_groups(&mut p) {
        let last = g.children.iter().rposition(|c| matches!(c, Node::Prop(_)));
        if let Some(i) = last {
            g.children.remove(i);
        }
        for c in &mut g.children {
            if let Node::Prop(pr) = c {
                pr.name = format!("Old {}", pr.name);
                if let ParamUi::Popup { options } = &mut pr.ui {
                    options.reverse();
                }
            }
        }
    }
    p
}

fn open(p: &Project) -> Project {
    let mut s = Session::default();
    s.replace_project(Project::from_json(&p.to_json_compact()).unwrap(), None);
    (*s.project).clone()
}

/// Every instance is current after opening, and opening again changes nothing.
fn assert_converged(p: &Project) {
    let mut q = p.clone();
    for g in effect_groups(&mut q) {
        let GroupKind::Effect { effect } = &g.kind else { continue };
        if let Some(spec) = crate::effects::find(effect) {
            assert!(crate::effects::migrate::is_current(spec, g), "{effect} not current after open");
        }
    }
    let mut again = p.clone();
    upgrade_effects(&mut again);
    assert!(again.same_content(p));
}

#[test]
fn opening_a_current_project_copies_no_layer() {
    let p = Project::from_json(&large(20).to_json_compact()).unwrap();
    let mut q = p.clone();
    upgrade_effects(&mut q);
    assert!(q.same_content(&p));
    for (id, it) in &p.items {
        let (ItemKind::Comp(a), Some(ItemKind::Comp(b))) = (&it.kind, q.items.get(id).map(|i| &i.kind)) else { continue };
        assert!(Arc::ptr_eq(a, b), "comp {id:?} copied");
    }
}

#[test]
fn opening_upgrades_only_the_stale_layers() {
    let p = Project::from_json(&large(5).to_json_compact()).unwrap();
    let cid = p.comps().next().map(|(id, _)| *id).unwrap();
    let mut q = p.clone();
    // Layer #3's blur saved without its last parameter.
    let fx = q.comp_mut(cid).unwrap().layers.get_mut(2).unwrap().props.sub_mut("effects").unwrap();
    let Node::Group(g) = &mut fx.children[0] else { panic!("no effect") };
    g.children.pop();
    let held = q.clone();
    upgrade_effects(&mut q);
    let (a, b) = (&held.comp(cid).unwrap().layers, &q.comp(cid).unwrap().layers);
    assert!(!a.shares(b, 2));
    assert_eq!((0..a.len()).filter(|&i| a.shares(b, i)).count(), a.len() - 1);
    assert_converged(&q);
}

/// 20-08: Bevel Alpha and Scatter imported from After Effects before EffectCraft knew them were
/// saved as plain groups that rendered nothing. Opening the project makes them native effects
/// with their values and keys; a third-party plug-in's group stays as it was.
#[test]
fn opening_turns_plain_ae_effects_into_native_effects() {
    let mut p = large(1);
    let cid = p.comps().next().map(|(id, _)| *id).unwrap();
    let fx = p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap().props.sub_mut("effects").unwrap();
    let group = |m: &str, uid: u64, params: &[(&str, effectcraft_keyframe::Value)]| {
        let mut g = PropGroup::new(uid, m, m);
        for (i, (pm, v)) in params.iter().enumerate() {
            g.children.push(Node::Prop(effectcraft_project::Property::new(uid + 1 + i as u64, pm, pm, v.clone())));
        }
        g
    };
    use effectcraft_keyframe::Value::Scalar;
    let mut bevel = group("ADBE Bevel Alpha", 900_000, &[("ADBE Bevel Alpha-0001", Scalar(4.0)), ("ADBE Bevel Alpha-0002", Scalar(-35.0))]);
    if let Some(Node::Prop(angle)) = bevel.children.get_mut(1) {
        angle.keys = vec![
            effectcraft_keyframe::Keyframe::new(effectcraft_time::Tick::ZERO, Scalar(-35.0)),
            effectcraft_keyframe::Keyframe::new(effectcraft_time::Tick::from_seconds_f64(1.0), Scalar(10.0)),
        ];
    }
    fx.children.push(bevel.into());
    fx.children.push(group("ADBE Scatter", 900_100, &[("ADBE Scatter-0001", Scalar(5.0)), ("ADBE Scatter-0002", Scalar(1.0))]).into());
    fx.children.push(group("PEDG2", 900_200, &[("PEDG2-0001", Scalar(1.0))]).into());
    let mut q = Project::from_json(&p.to_json_compact()).unwrap();
    q.fix_next_id();
    upgrade_effects(&mut q);
    let fx: Vec<&PropGroup> = q.comp(cid).unwrap().layers[0].props.sub("effects").unwrap().groups().collect();
    let bevel = fx.iter().find(|g| g.uid == 900_000).unwrap();
    assert_eq!(bevel.kind, GroupKind::Effect { effect: "ec.perspective.bevelalpha".into() });
    assert_eq!(bevel.prop("edgeThickness").unwrap().value, Scalar(4.0));
    assert_eq!(bevel.prop("lightAngle").unwrap().keys.iter().map(|k| k.value.clone()).collect::<Vec<_>>(), vec![Scalar(-35.0), Scalar(10.0)]);
    // Untouched parameters take their defaults.
    assert_eq!(bevel.prop("lightIntensity").unwrap().value, Scalar(0.4));
    let scatter = fx.iter().find(|g| g.uid == 900_100).unwrap();
    assert_eq!(scatter.kind, GroupKind::Effect { effect: "ec.stylize.scatter".into() });
    assert_eq!(scatter.prop("grain").unwrap().value, effectcraft_keyframe::Value::Enum(0));
    let glow = fx.iter().find(|g| g.uid == 900_200).unwrap();
    assert_eq!(glow.kind, GroupKind::Plain);
    assert_eq!(glow.children.len(), 1);
    // Unique uids after the rebuild, and a second open changes nothing.
    let mut uids = std::collections::HashSet::new();
    q.comp(cid).unwrap().layers[0].props.walk("", &mut |_, pr| assert!(uids.insert(pr.uid), "duplicate uid {}", pr.uid));
    let mut again = q.clone();
    upgrade_effects(&mut again);
    assert!(again.same_content(&q));
}

/// Writes the opened projects' JSON to `$EC_OPEN_DUMP` when set (compared byte for byte with
/// the code before the speed-up).
#[test]
fn opened_projects_are_upgraded_and_converge() {
    let demo = crate::demo::demo_project();
    let cases = [("demo", demo.clone()), ("demo-stale", stale(demo)), ("large-stale", stale(large(4)))];
    for (name, p) in cases {
        let opened = open(&p);
        assert_converged(&opened);
        if let Ok(dir) = std::env::var("EC_OPEN_DUMP") {
            std::fs::write(format!("{dir}/{name}.json"), opened.to_json()).unwrap();
        }
    }
    // A current project opens unchanged.
    let demo = Project::from_json(&crate::demo::demo_project().to_json_compact()).unwrap();
    let mut q = demo.clone();
    upgrade_effects(&mut q);
    assert!(q.same_content(&demo));
}

/// `cargo test -p effectcraft-engine --release --lib open_timing -- --ignored --nocapture`
#[test]
#[ignore]
fn open_timing() {
    let reps = 20;
    for (name, p) in [("current", large(300)), ("stale", stale(large(300)))] {
        let text = p.to_file_json().unwrap();
        let (mut parse, mut fix, mut upgrade, mut unique, mut whole) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for _ in 0..reps {
            let t = Instant::now();
            let q = Project::from_json(&text).unwrap();
            parse += t.elapsed().as_secs_f64();
            // A shared copy (its layers are also held elsewhere), then the unshared parse.
            let mut r = q.clone();
            let t = Instant::now();
            r.fix_next_id();
            fix += t.elapsed().as_secs_f64();
            let t = Instant::now();
            upgrade_effects(&mut r);
            upgrade += t.elapsed().as_secs_f64();
            drop(r);
            let mut q = q;
            let t = Instant::now();
            upgrade_effects(&mut q);
            unique += t.elapsed().as_secs_f64();
            let mut s = Session::default();
            let t = Instant::now();
            let q = Project::from_json(&text).unwrap();
            s.replace_project(q, None);
            whole += t.elapsed().as_secs_f64();
        }
        let ms = |x: f64| x * 1000.0 / reps as f64;
        println!(
            "{name}: {} bytes; from_json {:.2} ms, 2nd fix_next_id {:.3} ms, upgrade_effects shared {:.2} ms / unshared {:.2} ms, from_json+replace_project {:.2} ms",
            text.len(),
            ms(parse),
            ms(fix),
            ms(upgrade),
            ms(unique),
            ms(whole)
        );
    }
}
