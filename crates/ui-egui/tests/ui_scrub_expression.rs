//! Scrubbing the hot text of a property with an expression moves the value under the expression
//! by the drag, as in After Effects: Cmd 10× finer, Shift 10× coarser. The field shows the
//! expression's result, and writing that back each frame added a `wiggle`'s offset again on
//! every frame, so an angle ran away by hundreds of degrees whatever the modifier.

use effectcraft_engine::Session;
use effectcraft_ui_egui::EffectcraftApp;
use effectcraft_ui_egui::control::{self, ControlRequest, Outcome};
use egui_kittest::Harness;
use serde_json::{Value, json};

/// The expression's result: 60° on top of the value under it (a stand-in for a `wiggle` at a
/// time it is far from zero).
struct Offset;
impl effectcraft_render::ExprHost for Offset {
    fn eval(
        &self,
        _: &effectcraft_render::EvalCtx,
        _: &effectcraft_engine::project::Layer,
        _: &effectcraft_engine::project::Property,
        value: &effectcraft_engine::keyframe::Value,
    ) -> Result<effectcraft_engine::keyframe::Value, String> {
        Ok(value.with_components(&value.components().iter().map(|v| v + 60.0).collect::<Vec<_>>()))
    }
}

/// A layer whose Rotation (an angle: revolutions and degrees) has an expression, revealed in the
/// Timeline.
fn harness() -> (Harness<'static, EffectcraftApp>, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Scrub", "width": 320, "height": 180, "frameRate": 30, "duration": 10})).unwrap();
    s.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 80, "height": 80})).unwrap();
    let id = s.active_comp().unwrap().layers[0].id;
    s.expr = Some(std::sync::Arc::new(Offset));
    s.execute("prop.setExpression", json!({"layer": id.0, "path": "transform/rotation", "expression": "wiggle(1,100)"})).unwrap();
    s.execute("layer.select", json!({"layers": [id.0]})).unwrap();
    let app = EffectcraftApp::new(s);
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| app);
    h.run_steps(3);
    send(&mut h, "ui.menu.invoke", json!({"command": "timeline.reveal.rotation"}));
    (h, id.0)
}

fn send(h: &mut Harness<'_, EffectcraftApp>, method: &str, params: Value) {
    let ctx = h.ctx.clone();
    let (req, _rx) = ControlRequest::new(method, params);
    if let Outcome::Done(v) = control::handle(h.state_mut(), &ctx, &req) {
        assert!(v["ok"] != false, "{method}: {v}");
    }
    for e in h.state_mut().take_synthetic_input() {
        h.event(e);
        h.step();
    }
    h.run_steps(3);
}

fn rotation(h: &mut Harness<'_, EffectcraftApp>, layer: u64) -> f64 {
    let v = h.state_mut().session.execute("prop.get", json!({"layer": layer, "path": "transform/rotation"})).unwrap();
    v["value"].as_f64().unwrap()
}

#[test]
fn scrubbing_a_wiggled_angle_moves_it_by_the_drag_with_cmd_and_shift() {
    let (mut h, layer) = harness();
    let uid = h.state_mut().session.execute("prop.get", json!({"layer": layer, "path": "transform/rotation"})).unwrap()["uid"].as_u64().unwrap();
    let id = format!("timeline.prop.{uid}.value");
    let mut moved = vec![];
    for modifiers in [json!({}), json!({"command": true}), json!({"shift": true})] {
        h.state_mut().session.execute("prop.set", json!({"layer": layer, "path": "transform/rotation", "value": 0.0})).unwrap();
        h.run_steps(3);
        let r = h.state().auto.find_current(&id).expect("the degrees field").rect;
        let (x, y) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
        send(&mut h, "ui.drag", json!({"from": {"x": x, "y": y}, "to": {"x": x + 100.0, "y": y}, "steps": 20, "modifiers": modifiers}));
        moved.push(rotation(&mut h, layer));
    }
    let (plain, fine, coarse) = (moved[0], moved[1], moved[2]);
    assert!(plain > 10.0 && plain < 100.0, "a plain drag moves the value under the expression by the drag: {moved:?}");
    assert!((fine - plain / 10.0).abs() < 1e-6, "Cmd scrubs 10× finer: {moved:?}");
    assert!((coarse - plain * 10.0).abs() < 1e-6, "Shift scrubs 10× coarser: {moved:?}");
}
