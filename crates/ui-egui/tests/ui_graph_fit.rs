//! Opening the Graph Editor frames the graphs, as in After Effects: Fit Selection to View when
//! keys are selected (a key selected in the Timeline shows its property's graph even when the
//! layer isn't selected), else Fit All Graphs to View. It used to open on an empty 0–100 range
//! or with the curves off-view.

use effectcraft_engine::Session;
use effectcraft_ui_egui::EffectcraftApp;
use egui_kittest::Harness;
use serde_json::json;

/// A 20 s comp whose Box layer has Rotation keys 300° at 8 s and 720° at 12 s; the Timeline is
/// zoomed in on 0–4 s, so neither key is in view; the value graph shows (the default is
/// automatic). Returns the harness and Rotation's uid.
fn harness(select_layer: bool) -> (Harness<'static, EffectcraftApp>, u64, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Fit", "width": 320, "height": 180, "frameRate": 30, "duration": 20})).unwrap();
    s.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 80, "height": 80})).unwrap();
    let id = s.active_comp().unwrap().layers[0].id;
    for (t, v) in [(8.0, 300.0), (12.0, 720.0)] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/rotation", "time": t, "value": v})).unwrap();
    }
    let uid = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/rotation").unwrap().uid;
    s.execute("layer.select", json!({"layers": if select_layer { vec![id.0] } else { vec![] }})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.state_mut().ui.timeline.graph_mode = "value".into();
    h.state_mut().ui.timeline.pps = Some(300.0);
    h.state_mut().ui.timeline.start = 0.0;
    h.run_steps(3);
    (h, id.0, uid)
}

fn open_graph(h: &mut Harness<'_, EffectcraftApp>) {
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.graphEditor", json!({})).unwrap();
    h.run_steps(4);
    assert!(h.state().ui.timeline.graph_editor);
}

/// Every key of the property sits inside the plot, and the value range spans `lo..=hi`.
fn assert_framed(h: &Harness<'_, EffectcraftApp>, uid: u64, keys: &[usize], lo: f64, hi: f64) {
    let plot = h.state().auto.find("timeline.graph").expect("graph plot").rect;
    for i in keys {
        let k = h.state().auto.find(&format!("timeline.graph.key.{uid}.0.{i}")).unwrap_or_else(|| panic!("graph key {i} not drawn")).rect;
        let (cx, cy) = (k[0] + k[2] / 2.0, k[1] + k[3] / 2.0);
        assert!(cx > plot[0] && cx < plot[0] + plot[2] && cy > plot[1] && cy < plot[1] + plot[3], "key {i} at ({cx}, {cy}) outside plot {plot:?}");
    }
    let (a, b) = h.state().ui.timeline.graph_range.expect("graph range");
    assert!(a <= lo && b >= hi, "value range {a}..{b} doesn't contain {lo}..{hi}");
}

#[test]
fn opening_the_graph_editor_fits_the_selected_key() {
    let (mut h, layer, uid) = harness(false);
    h.state_mut().session.execute("keys.select", json!({"keys": [{"layer": layer, "prop": uid, "time": 12.0}]})).unwrap();
    h.run_steps(2);
    open_graph(&mut h);
    // A lone key also frames its neighbour, so its curve shows.
    assert_framed(&h, uid, &[0, 1], 300.0, 720.0);
    assert!(!h.state().ui.timeline.graph_auto_zoom, "Fit Selection turns auto-zoom off");
}

#[test]
fn opening_the_graph_editor_with_no_keys_selected_fits_all_graphs() {
    let (mut h, _, uid) = harness(true);
    open_graph(&mut h);
    assert!(h.state().ui.timeline.pps.is_some(), "frame the animation, not the whole comp");
    assert!(h.state().ui.timeline.start > 7.0 && h.state().ui.timeline.start < 8.0);
    assert_framed(&h, uid, &[0, 1], 300.0, 720.0);
    // Zooming away and toggling the Graph Editor off and on frames it again.
    h.state_mut().ui.timeline.pps = Some(300.0);
    h.state_mut().ui.timeline.start = 0.0;
    open_graph_toggle_twice(&mut h);
    assert_framed(&h, uid, &[0, 1], 300.0, 720.0);
}

fn open_graph_toggle_twice(h: &mut Harness<'_, EffectcraftApp>) {
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.graphEditor", json!({})).unwrap();
    h.run_steps(2);
    assert!(!h.state().ui.timeline.graph_editor);
    open_graph(h);
}

/// Regression: an eased Position move opens on the speed graph, where both keys sit at speed 0
/// and the curve peaks between them. Fit Selection framed only the keys (a flat -1..1 range) and
/// left the curve off the top; it frames the curve between the selected keys too.
#[test]
fn opening_the_graph_editor_frames_the_speed_curve_between_eased_keys() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Ease", "width": 320, "height": 180, "frameRate": 30, "duration": 10})).unwrap();
    s.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 40, "height": 40})).unwrap();
    let id = s.active_comp().unwrap().layers[0].id;
    for (t, x) in [(0.0, 0.0), (1.0, 300.0)] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/position", "time": t, "value": [x, 90.0, 0.0]})).unwrap();
    }
    let uid = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/position").unwrap().uid;
    s.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": 0.0}, {"layer": id.0, "prop": uid, "time": 1.0}]})).unwrap();
    s.execute("keys.velocity", json!({"inSpeed": 0, "inInfluence": 85, "outSpeed": 0, "outInfluence": 35})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(3);
    open_graph(&mut h);
    // 300 px in 1 s: the average speed is 300 px/s, so the peak is higher; the keys are at 0.
    assert_framed(&h, uid, &[0, 1], 0.0, 300.0);
}

/// Leaving the Graph Editor puts back the time zoom from before it opened: its fit used to stay,
/// so the layer bars came back zoomed in on the keys.
#[test]
fn closing_the_graph_editor_restores_the_time_zoom() {
    let (mut h, _, _) = harness(true);
    open_graph(&mut h);
    assert!(h.state().ui.timeline.pps.is_some(), "fit zooms into the keys");
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.graphEditor", json!({})).unwrap();
    h.run_steps(2);
    assert!(!h.state().ui.timeline.graph_editor);
    assert_eq!((h.state().ui.timeline.start, h.state().ui.timeline.pps), (0.0, Some(300.0)));
}

/// The graph type chosen with the buttons is remembered (Preferences `graphType`): a new window
/// (the next launch) opens on it. The default is automatic.
#[test]
fn the_chosen_graph_type_is_remembered() {
    assert_eq!(Session::default().prefs.graph_type, "auto");
    assert_eq!(EffectcraftApp::new(Session::default()).ui.timeline.graph_mode, "auto");
    let (mut h, _, _) = harness(true);
    h.state_mut().ui.timeline.graph_mode = "speed".into();
    open_graph(&mut h);
    for id in ["timeline.graph.typeOptions", "timeline.graph.valueGraph"] {
        let b = h.state().auto.find(id).expect("graph control").rect;
        let pos = egui::pos2(b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
        h.event(egui::Event::PointerMoved(pos));
        h.step();
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
            h.step();
        }
        h.run_steps(2);
    }
    h.run_steps(2);
    assert_eq!(h.state().ui.timeline.graph_mode, "value");
    assert_eq!(h.state().session.prefs.graph_type, "value");
    let s = Session { prefs: h.state().session.prefs.clone(), ..Default::default() };
    assert_eq!(EffectcraftApp::new(s).ui.timeline.graph_mode, "value");
}

#[test]
fn graph_set_keeps_an_unselected_layers_property_visible() {
    let (mut h, layer, uid) = harness(false);
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": uid, "value": true})).unwrap();
    h.state_mut().session.execute("layer.select", json!({"layers": []})).unwrap();
    h.state_mut().session.execute("keys.select", json!({"keys": []})).unwrap();
    h.state_mut().ui.timeline.graph_show_set = true;
    open_graph(&mut h);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{uid}.0.0")).is_some());
    h.state_mut().ui.timeline.graph_show_set = false;
    h.run_steps(3);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{uid}.0.0")).is_none());
}

#[test]
fn wheel_pans_graph_values_without_scrolling_the_outline() {
    let (mut h, _, _) = harness(true);
    open_graph(&mut h);
    h.state_mut().ui.timeline.graph_auto_zoom = false;
    h.state_mut().ui.timeline.graph_range = Some((0.0, 1000.0));
    h.run_steps(3);
    let plot = h.state().auto.find("timeline.graph").unwrap().rect;
    let before_rows = h.state().ui.timeline.scroll_y;
    h.event(egui::Event::PointerMoved(egui::pos2(plot[0] + plot[2] * 0.5, plot[1] + plot[3] * 0.5)));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 60.0),
        modifiers: Default::default(),
        phase: egui::TouchPhase::Move,
    });
    h.run_steps(3);
    let (lo, hi) = h.state().ui.timeline.graph_range.unwrap();
    // Scrolling down moves the curve down, as a Hand drag does: the view shows higher values.
    assert!(lo > 0.0, "{lo}");
    assert!((hi - lo - 1000.0).abs() < 1e-6);
    assert_eq!(h.state().ui.timeline.scroll_y, before_rows);
}

#[test]
fn hand_and_middle_drag_pan_without_editing_keys() {
    use egui::{Event, PointerButton, pos2, vec2};
    for (button, space) in [(PointerButton::Primary, false), (PointerButton::Middle, false), (PointerButton::Primary, true)] {
        let (mut h, layer, uid) = harness(true);
        open_graph(&mut h);
        h.state_mut().ui.timeline.graph_auto_zoom = false;
        h.state_mut().ui.timeline.graph_range = Some((0.0, 1000.0));
        h.state_mut().ui.timeline.pps = Some(100.0);
        h.state_mut().ui.timeline.start = 5.0;
        if button == PointerButton::Primary && !space {
            h.state_mut().ui.tool = effectcraft_ui_egui::state::Tool::Hand;
        }
        h.run_steps(3);
        let plot = h.state().auto.find("timeline.graph").unwrap().rect;
        let from = pos2(plot[0] + plot[2] * 0.5, plot[1] + plot[3] * 0.5);
        let before = h.state().session.active_comp().unwrap().layer(effectcraft_engine::project::LayerId(layer)).unwrap().props.find(uid).unwrap().keys.clone();
        h.event(Event::PointerMoved(from));
        h.step();
        if space {
            h.event(Event::Key { key: egui::Key::Space, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() });
            h.step();
        }
        h.event(Event::PointerButton { pos: from, button, pressed: true, modifiers: Default::default() });
        h.step();
        for i in 1..=10 {
            h.event(Event::PointerMoved(from + vec2(i as f32 * 4.0, i as f32 * 3.0)));
            h.step();
        }
        h.event(Event::PointerButton { pos: from + vec2(40.0, 30.0), button, pressed: false, modifiers: Default::default() });
        h.run_steps(3);
        if space {
            h.event(Event::Key { key: egui::Key::Space, physical_key: None, pressed: false, repeat: false, modifiers: Default::default() });
            h.run_steps(3);
        }
        assert!(!h.state().playback.playing, "panning must not start playback");
        assert!(h.state().ui.timeline.start < 5.0, "{button:?} pans time");
        assert!(h.state().ui.timeline.graph_range.unwrap().0 > 0.0, "{button:?} pans values");
        let after = &h.state().session.active_comp().unwrap().layer(effectcraft_engine::project::LayerId(layer)).unwrap().props.find(uid).unwrap().keys;
        assert_eq!(&before, after);
    }
}

#[test]
fn auto_height_blocks_manual_value_zoom() {
    let (mut h, _, _) = harness(true);
    open_graph(&mut h);
    let plot = h.state().auto.find("timeline.graph").unwrap().rect;
    let before = h.state().ui.timeline.graph_range;
    h.event(egui::Event::PointerMoved(egui::pos2(plot[0] + plot[2] * 0.5, plot[1] + plot[3] * 0.5)));
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 60.0),
        modifiers: egui::Modifiers::COMMAND,
        phase: egui::TouchPhase::Move,
    });
    h.run_steps(3);
    assert!(h.state().ui.timeline.graph_auto_zoom);
    assert_eq!(h.state().ui.timeline.graph_range, before);
}

fn click_graph(h: &mut Harness<'_, EffectcraftApp>, id: &str, button: egui::PointerButton) {
    let r = h.state().auto.find(id).unwrap_or_else(|| panic!("missing {id}")).rect;
    let pos = egui::pos2(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
    h.event(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos, button, pressed, modifiers: Default::default() });
        h.step();
    }
    h.run_steps(2);
}

#[test]
fn graph_set_only_excludes_selected_unpinned_curves_and_can_be_empty() {
    let (mut h, layer, rotation) = harness(true);
    h.state_mut().session.execute("prop.addKey", json!({"layer": layer, "path": "transform/opacity", "time": 8.0, "value": 40})).unwrap();
    let opacity = h.state().session.active_comp().unwrap().layers[0].props.prop("transform/opacity").unwrap().uid;
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": rotation, "value": true})).unwrap();
    open_graph(&mut h);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{opacity}.0.0")).is_some());
    click_graph(&mut h, "timeline.graph.propertyOptions", egui::PointerButton::Primary);
    click_graph(&mut h, "timeline.graph.setOnly", egui::PointerButton::Primary);
    assert!(h.state().ui.timeline.graph_set_only);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{rotation}.0.0")).is_some());
    assert!(h.state().auto.find(&format!("timeline.graph.key.{opacity}.0.0")).is_none());
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": rotation, "value": false})).unwrap();
    h.run_steps(3);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{rotation}.0.0")).is_none());
    click_graph(&mut h, "timeline.graph.propertyOptions", egui::PointerButton::Primary);
    click_graph(&mut h, "timeline.graph.showAnimated", egui::PointerButton::Primary);
    assert!(!h.state().ui.timeline.graph_set_only);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{opacity}.0.0")).is_some());
}

#[test]
fn graph_key_context_menu_preserves_selected_group_and_selects_unselected_target() {
    for group in [false, true] {
        let (mut h, layer, uid) = harness(true);
        let keys = if group { vec![8.0, 12.0] } else { vec![12.0] };
        h.state_mut()
            .session
            .execute("keys.select", json!({"keys": keys.iter().map(|time| json!({"layer": layer, "prop": uid, "time": time})).collect::<Vec<_>>()}))
            .unwrap();
        open_graph(&mut h);
        h.state_mut().ui.timeline.graph_transform_box = false;
        h.run_steps(2);
        click_graph(&mut h, &format!("timeline.graph.key.{uid}.0.0"), egui::PointerButton::Secondary);
        let selected = &h.state().session.state.selected_keys;
        assert_eq!(selected.len(), if group { 2 } else { 1 });
        assert!(selected.iter().any(|key| key.time.seconds() == 8.0));
        click_graph(&mut h, "timeline.graph.keyMenu.hold", egui::PointerButton::Primary);
        let pr = h.state().session.active_comp().unwrap().layers[0].props.find(uid).unwrap();
        assert_eq!(pr.keys[0].out_interp, effectcraft_engine::keyframe::Interp::Hold);
        assert_eq!(pr.keys[1].out_interp == effectcraft_engine::keyframe::Interp::Hold, group);
        assert!(h.state_mut().session.undo());
        let pr = h.state().session.active_comp().unwrap().layers[0].props.find(uid).unwrap();
        assert_ne!(pr.keys[0].out_interp, effectcraft_engine::keyframe::Interp::Hold);
    }
}

#[test]
fn graph_keys_open_existing_value_velocity_and_interpolation_dialogs() {
    for (mode, menu, expected) in [
        ("value", Some("editValue"), effectcraft_ui_egui::Dialog::Form),
        ("value", Some("velocity"), effectcraft_ui_egui::Dialog::KeyVelocity),
        ("value", Some("interpolation"), effectcraft_ui_egui::Dialog::KeyInterpolation),
        ("value", None, effectcraft_ui_egui::Dialog::Form),
        ("speed", None, effectcraft_ui_egui::Dialog::KeyVelocity),
    ] {
        let (mut h, _, uid) = harness(true);
        h.state_mut().ui.timeline.graph_mode = mode.into();
        open_graph(&mut h);
        let key = format!("timeline.graph.key.{uid}.0.0");
        if let Some(menu) = menu {
            click_graph(&mut h, &key, egui::PointerButton::Secondary);
            click_graph(&mut h, &format!("timeline.graph.keyMenu.{menu}"), egui::PointerButton::Primary);
        } else {
            // Both clicks reach egui in one frame: a double click needs them closer in time
            // than the harness's frames are apart.
            let r = h.state().auto.find(&key).unwrap_or_else(|| panic!("missing {key}")).rect;
            let pos = egui::pos2(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
            h.input_mut().events.push(egui::Event::PointerMoved(pos));
            h.step();
            for pressed in [true, false, true, false] {
                h.input_mut().events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
            }
            h.step();
            h.run_steps(3);
        }
        assert_eq!(h.state().dialog, Some(expected), "{mode} {menu:?}");
    }
}

#[test]
fn graph_fit_empty_or_static_restores_comp_and_single_key_stays_visible() {
    let (mut h, layer, uid) = harness(false);
    open_graph(&mut h);
    assert_eq!(h.state().ui.timeline.pps, None);
    assert_eq!(h.state().ui.timeline.start, 0.0);
    h.state_mut().session.execute("prop.addKey", json!({"layer": layer, "path": "transform/opacity", "time": 9.0, "value": 40})).unwrap();
    let opacity = h.state().session.active_comp().unwrap().layers[0].props.prop("transform/opacity").unwrap().uid;
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": opacity, "value": true})).unwrap();
    h.state_mut().ui.timeline.graph_set_only = true;
    click_graph(&mut h, "timeline.graph.fit", egui::PointerButton::Primary);
    assert_framed(&h, opacity, &[0], 40.0, 40.0);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{uid}.0.0")).is_none());
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": opacity, "value": false})).unwrap();
    let scale = h.state().session.active_comp().unwrap().layers[0].props.prop("transform/scale").unwrap().uid;
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": scale, "value": true})).unwrap();
    click_graph(&mut h, "timeline.graph.fit", egui::PointerButton::Primary);
    assert_eq!(h.state().ui.timeline.pps, None);
    assert!(h.state().ui.timeline.graph_range.unwrap().0.is_finite());
}

#[test]
fn graph_set_only_state_roundtrips_and_old_settings_default_off() {
    let state = effectcraft_ui_egui::state::TimelineState { graph_set_only: true, ..Default::default() };
    let mut saved = serde_json::to_value(&state).unwrap();
    let restored: effectcraft_ui_egui::state::TimelineState = serde_json::from_value(saved.clone()).unwrap();
    assert!(restored.graph_set_only);
    saved.as_object_mut().unwrap().remove("graph_set_only");
    let legacy: effectcraft_ui_egui::state::TimelineState = serde_json::from_value(saved).unwrap();
    assert!(!legacy.graph_set_only);
}

#[test]
fn graph_fit_keyless_expression_uses_the_layers_active_interval() {
    let (mut h, layer, _) = harness(false);
    let cid = h.state().session.state.active_comp.unwrap();
    let l = std::sync::Arc::make_mut(&mut h.state_mut().session.project).comp_mut(cid).unwrap().layer_mut(effectcraft_engine::project::LayerId(layer)).unwrap();
    l.in_point = effectcraft_engine::time::Tick::from_seconds_f64(3.0);
    l.out_point = effectcraft_engine::time::Tick::from_seconds_f64(6.0);
    h.state_mut().session.execute("prop.setExpression", json!({"layer": layer, "path": "transform/opacity", "expression": "time * 10"})).unwrap();
    let uid = h.state().session.active_comp().unwrap().layers[0].props.prop("transform/opacity").unwrap().uid;
    h.state_mut().session.execute("prop.graphPin", json!({"layer": layer, "prop": uid, "value": true})).unwrap();
    h.state_mut().ui.timeline.graph_set_only = true;
    open_graph(&mut h);
    assert!(h.state().ui.timeline.start > 2.0 && h.state().ui.timeline.start < 3.0);
    let plot = h.state().auto.find("timeline.graph").unwrap().rect;
    let visible_seconds = f64::from(plot[2]) / h.state().ui.timeline.pps.unwrap();
    assert!((3.0..5.0).contains(&visible_seconds));
}

#[test]
fn expression_comparison_menu_is_independent_of_the_other_graph_reference() {
    struct Offset;
    impl effectcraft_render::ExprHost for Offset {
        fn eval(
            &self,
            _: &effectcraft_render::EvalCtx,
            _: &effectcraft_engine::project::Layer,
            _: &effectcraft_engine::project::Property,
            value: &effectcraft_engine::keyframe::Value,
        ) -> Result<effectcraft_engine::keyframe::Value, String> {
            Ok(value.with_components(&value.components().iter().map(|v| v + 200.0).collect::<Vec<_>>()))
        }
    }
    let (mut h, layer, uid) = harness(true);
    h.state_mut().session.expr = Some(std::sync::Arc::new(Offset));
    h.state_mut().session.execute("prop.setExpression", json!({"layer": layer, "prop": uid, "expression": "offset"})).unwrap();
    open_graph(&mut h);
    let revision = h.state().session.revision;
    assert!(!h.state().ui.timeline.graph_expression_overlay);
    assert!(h.state().auto.find("timeline.graph.expressionLegend").is_none());
    click_graph(&mut h, "timeline.graph.typeOptions", egui::PointerButton::Primary);
    click_graph(&mut h, "timeline.graph.expressionOverlay", egui::PointerButton::Primary);
    assert!(h.state().ui.timeline.graph_expression_overlay);
    assert!(!h.state().ui.timeline.graph_reference);
    assert!(h.state().auto.find("timeline.graph.expressionLegend").is_some());
    click_graph(&mut h, "timeline.graph.typeOptions", egui::PointerButton::Primary);
    click_graph(&mut h, "timeline.graph.reference", egui::PointerButton::Primary);
    assert!(h.state().ui.timeline.graph_reference);
    assert!(h.state().ui.timeline.graph_expression_overlay);
    click_graph(&mut h, "timeline.graph.typeOptions", egui::PointerButton::Primary);
    click_graph(&mut h, "timeline.graph.expressionOverlay", egui::PointerButton::Primary);
    assert!(!h.state().ui.timeline.graph_expression_overlay);
    assert!(h.state().ui.timeline.graph_reference);
    assert!(h.state().auto.find("timeline.graph.expressionLegend").is_none());
    assert_eq!(h.state().session.revision, revision, "comparison must not edit the document");
}

#[test]
fn expression_comparison_defaults_off_and_roundtrips_without_changing_legacy_reference() {
    let mut state = effectcraft_ui_egui::state::TimelineState::default();
    assert!(!state.graph_expression_overlay);
    state.graph_expression_overlay = true;
    state.graph_reference = true;
    let mut saved = serde_json::to_value(&state).unwrap();
    let roundtrip: effectcraft_ui_egui::state::TimelineState = serde_json::from_value(saved.clone()).unwrap();
    assert!(roundtrip.graph_expression_overlay && roundtrip.graph_reference);
    saved.as_object_mut().unwrap().remove("graph_expression_overlay");
    let legacy: effectcraft_ui_egui::state::TimelineState = serde_json::from_value(saved).unwrap();
    assert!(!legacy.graph_expression_overlay);
    assert!(legacy.graph_reference);
}
