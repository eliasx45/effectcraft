//! Text Gradient Fill (EffectCraft extension) in the engine: Animate Text ▸ Gradient Fill adds
//! its four properties, a whole-animator preset (.ecpreset v2) saves and restores them with the
//! stops, and Gradient Start takes an expression like any animator property.

use effectcraft_keyframe::{Gradient, Value as KV};
use effectcraft_project::{Expression, LayerId, PropGroup};
use serde_json::json;

use crate::Session;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ec-text-gradient-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn animators(s: &Session, l: u64) -> Vec<PropGroup> {
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
    layer
        .props
        .group("text/animators")
        .unwrap()
        .groups()
        .cloned()
        .map(|mut g| {
            let mut zero = 0;
            g.reassign_uids(&mut zero);
            g
        })
        .collect()
}

fn stops() -> Gradient {
    Gradient {
        colors: vec![(0.0, [1.0, 0.85, 0.0, 1.0]), (0.4, [1.0, 0.48, 0.0, 1.0]), (1.0, [0.88, 0.06, 0.06, 1.0])],
        opacities: vec![(0.0, 1.0), (1.0, 0.5)],
        ..Default::default()
    }
}

/// A text layer with a Gradient Fill animator (custom stops, End at 80,40, Radial): (session,
/// layer, animator).
fn session() -> (Session, u64, u64) {
    let mut s = Session { expr: Some(std::sync::Arc::new(effectcraft_expr::Expressions)), ..Default::default() };
    s.execute("comp.new", json!({"name": "G", "width": 320, "height": 180, "frameRate": 25, "duration": 4.0})).unwrap();
    let l = s.execute("layer.newText", json!({"text": "TESTING THIS ANIMATION"})).unwrap()["layer"].as_u64().unwrap();
    let a = s.execute("layer.addTextAnimator", json!({"layer": l, "property": "gradientFill"})).unwrap()["animator"].as_u64().unwrap();
    let anim = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.find_group(a).unwrap().clone();
    let uid = |m: &str| anim.prop(&format!("properties/{m}")).unwrap_or_else(|| panic!("no {m}")).uid;
    s.execute("prop.set", json!({"layer": l, "prop": uid("gradientFillColors"), "value": stops()})).unwrap();
    s.execute("prop.set", json!({"layer": l, "prop": uid("gradientEnd"), "value": [80.0, 40.0]})).unwrap();
    s.execute("prop.set", json!({"layer": l, "prop": uid("gradientType"), "value": 1})).unwrap();
    (s, l, a)
}

#[test]
fn animate_text_gradient_fill_adds_its_properties() {
    let (s, l, _) = session();
    let a = &animators(&s, l)[0];
    let props = a.sub("properties").unwrap();
    let ids: Vec<&str> = props.props().map(|p| p.match_id.as_str()).collect();
    assert_eq!(ids, effectcraft_project::build::GRADIENT_FILL_PROPS);
    assert!(matches!(&props.get("gradientFillColors").unwrap().value, KV::Gradient(g) if **g == stops()));
    assert_eq!(props.get("gradientStart").unwrap().value, KV::Vec2([0.0, 50.0]));
}

#[test]
fn a_preset_carries_the_gradient_fill_with_its_stops() {
    let path = temp_dir("preset").join("Warm Word.ecpreset").to_string_lossy().to_string();
    let (mut s, l, a) = session();
    s.state.selected_props = vec![(LayerId(l), a)];
    let r = s.execute("anim.savePreset", json!({"path": path})).unwrap();
    assert_eq!(r["animators"], json!(1), "{r:#}");
    let b = s.execute("layer.newText", json!({"text": "Fresh"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("anim.applyPreset", json!({"path": path, "layers": [b]})).unwrap();
    assert_eq!(animators(&s, b), animators(&s, l), "the same Gradient Fill: stops, End, Radial");
}

#[test]
fn gradient_start_takes_an_expression() {
    let (mut s, l, a) = session();
    let start = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.find_group(a).unwrap().prop("properties/gradientStart").unwrap().uid;
    s.execute("prop.setExpression", json!({"layer": l, "prop": start, "expression": "[25, 10 * 5]"})).unwrap();
    let cid = s.active_comp_id().unwrap();
    let comp = s.active_comp().unwrap();
    let layer = comp.layer(LayerId(l)).unwrap();
    let mut ctx = effectcraft_render::EvalCtx::new(&s.project, cid, comp, s.time());
    ctx.expr = s.expr.as_deref();
    assert_eq!(layer.props.find(start).unwrap().expr, Some(Expression { text: "[25, 10 * 5]".into(), enabled: true }));
    assert_eq!(ctx.value(layer, layer.props.find(start).unwrap()), KV::Vec2([25.0, 50.0]));
    // The renderer draws the group's Start there: a quarter across its box.
    let geom = effectcraft_render::text::text_geom(&ctx, layer).unwrap();
    let g = geom.grad_groups.first().unwrap();
    assert!((g.start[0] - (g.bbox[0] + 0.25 * (g.bbox[2] - g.bbox[0]))).abs() < 1e-9, "{g:?}");
}
