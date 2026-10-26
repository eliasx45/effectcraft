//! Headless checks for Timeline keyframes as a person uses them: dragging keys over many frames
//! in one gesture (the drag used to end after the first frame), Shift-snapping, Ctrl+C /
//! Ctrl+V, which the windowing layer delivers as clipboard events rather than key presses.

use effectcraft_engine::Session;
use effectcraft_engine::project::LayerId;
use effectcraft_ui_egui::EffectcraftApp;
use egui::{Event, Pos2, pos2};
use egui_kittest::Harness;
use serde_json::json;

/// A 4 s, 30 fps comp with a Box layer whose Opacity has keys at 0 s and 1 s, revealed in the
/// Timeline; returns the harness, the layer and the property uid.
fn harness() -> (Harness<'static, EffectcraftApp>, LayerId, u64) {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Keys", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    s.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 80, "height": 80})).unwrap();
    let id = s.active_comp().unwrap().layers[0].id;
    for (t, v) in [(0.0, 0.0), (1.0, 100.0)] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/opacity", "time": t, "value": v})).unwrap();
    }
    let uid = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().uid;
    s.execute("layer.select", json!({"layers": [id.0]})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    // Auto-Select Graph Type (the Graph Editor's default is the speed graph): Opacity shows its
    // value graph.
    h.state_mut().ui.timeline.graph_mode = "auto".into();
    h.run_steps(3);
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.reveal.opacity", json!({})).unwrap();
    h.run_steps(4);
    (h, id, uid)
}

fn key_times(h: &Harness<'_, EffectcraftApp>, id: LayerId) -> Vec<f64> {
    let l = h.state().session.active_comp().unwrap().layer(id).unwrap().clone();
    l.props.prop("transform/opacity").unwrap().keys.iter().map(|k| (k.time.seconds() * 30.0).round() / 30.0).collect()
}

/// Screen centres of the property's keys, left to right.
fn keys(h: &Harness<'_, EffectcraftApp>, uid: u64) -> Vec<Pos2> {
    let mut ks: Vec<Pos2> =
        h.state().auto.query(&format!("timeline.key.{uid}.")).iter().map(|e| pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)).collect();
    ks.sort_by(|a, b| a.x.total_cmp(&b.x));
    ks
}

/// Press at `from`, move to `to` in small steps (a real drag), release.
fn drag(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2, modifiers: egui::Modifiers) {
    h.input_mut().events.push(Event::PointerMoved(from));
    h.step();
    h.input_mut().events.push(Event::ModifiersChanged(modifiers));
    h.input_mut().events.push(Event::PointerButton { pos: from, button: egui::PointerButton::Primary, pressed: true, modifiers });
    h.step();
    for i in 1..=24 {
        h.input_mut().events.push(Event::PointerMoved(from + (to - from) * (i as f32 / 24.0)));
        h.step();
    }
    h.input_mut().events.push(Event::PointerButton { pos: to, button: egui::PointerButton::Primary, pressed: false, modifiers });
    h.step();
    h.input_mut().events.push(Event::ModifiersChanged(Default::default()));
    h.run_steps(2);
}

#[test]
fn a_key_drag_follows_the_pointer_for_its_whole_length() {
    let (mut h, id, uid) = harness();
    let ks = keys(&h, uid);
    assert_eq!(ks.len(), 2);
    let px_per_s = ks[1].x - ks[0].x;
    // Drag the 1 s key to where 2.5 s is: many frames, one gesture.
    let to = pos2(ks[1].x + px_per_s * 1.5, ks[1].y);
    drag(&mut h, ks[1], to, Default::default());
    let ts = key_times(&h, id);
    assert!((ts[1] - 2.5).abs() <= 2.0 / 30.0, "the key follows the whole drag: {ts:?}");
    let steps = h.state().session.history.undo.iter().filter(|(l, _)| l == "Move Keyframes").count();
    assert_eq!(steps, 1, "one undo step per drag");
    // Shift snaps it to the current time indicator (at 0.5 s), from a few pixels away.
    h.state_mut().session.set_time(effectcraft_engine::time::Tick::from_seconds_f64(0.5));
    h.run_steps(2);
    let ks = keys(&h, uid);
    let near = pos2(ks[0].x + px_per_s * 0.5 + 4.0, ks[1].y);
    drag(&mut h, ks[1], near, egui::Modifiers { shift: true, ..Default::default() });
    assert_eq!(key_times(&h, id)[1], 0.5, "snapped to the current time");
}

#[test]
fn ctrl_c_and_ctrl_v_copy_and_paste_keys_at_the_current_time() {
    let (mut h, id, uid) = harness();
    let ks = keys(&h, uid);
    // Click the 1 s key to select it, Ctrl+C (a clipboard event, not a key press).
    h.input_mut().events.push(Event::PointerMoved(ks[1]));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: ks[1], button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: ks[1], button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.run_steps(2);
    assert_eq!(h.state().session.state.selected_keys.len(), 1);
    h.input_mut().events.push(Event::Copy);
    h.step();
    let copied: Vec<String> = h
        .output()
        .platform_output
        .commands
        .iter()
        .filter_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(copied, ["EffectCraft: 1 keyframe"], "the system clipboard is filled, so Ctrl+V sends a paste event");
    assert!(h.state().session.state.clip_is_keys);
    // Move to 3 s, Ctrl+V: the key lands there.
    h.state_mut().session.set_time(effectcraft_engine::time::Tick::from_seconds_f64(3.0));
    h.input_mut().events.push(Event::Paste("EffectCraft: 1 keyframe".into()));
    h.run_steps(2);
    assert_eq!(key_times(&h, id), vec![0.0, 1.0, 3.0]);
}

fn click_with(h: &mut Harness<'_, EffectcraftApp>, p: Pos2, modifiers: egui::Modifiers) {
    h.input_mut().events.push(Event::PointerMoved(p));
    h.step();
    h.input_mut().events.push(Event::ModifiersChanged(modifiers));
    h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers });
    h.step();
    h.input_mut().events.push(Event::ModifiersChanged(Default::default()));
    h.run_steps(2);
}

#[test]
fn copy_and_paste_key_subsets_across_position_rotation_and_scale() {
    for graph in [false, true] {
        let (mut h, id, _) = harness();
        let paths = ["transform/position", "transform/rotation", "transform/scale"];
        for (path, times, values) in [
            (paths[0], [0.5, 1.0], [json!([100, 200, 0]), json!([300, 400, 0])]),
            (paths[1], [0.5, 1.5], [json!(500), json!(900)]),
            (paths[2], [0.0, 1.0], [json!([1500, 1600, 100]), json!([1700, 1800, 100])]),
        ] {
            for (time, value) in times.into_iter().zip(values) {
                h.state_mut().session.execute("prop.addKey", json!({"layer": id.0, "path": path, "time": time, "value": value})).unwrap();
            }
        }
        let uids: Vec<u64> = paths.iter().map(|path| h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop(path).unwrap().uid).collect();
        let ctx = h.ctx.clone();
        for command in ["timeline.reveal.position", "timeline.revealAdd.rotation", "timeline.revealAdd.scale"] {
            effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, command, json!({})).unwrap();
        }
        if graph {
            h.state_mut().session.execute("edit.deselectAll", json!({})).unwrap();
            for uid in &uids {
                h.state_mut().session.execute("prop.select", json!({"layer": id.0, "prop": uid, "add": true, "selectKeys": false})).unwrap();
            }
            h.state_mut().ui.timeline.graph_editor = true;
            h.state_mut().ui.timeline.graph_mode = "value".into();
        }
        h.run_steps(4);
        let key_positions = |h: &Harness<'_, EffectcraftApp>, uid: u64| {
            if !graph {
                return keys(h, uid);
            }
            let mut positions: Vec<Pos2> = h
                .state()
                .auto
                .query(&format!("timeline.graph.key.{uid}.0."))
                .iter()
                .map(|e| pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0))
                .collect();
            positions.sort_by(|a, b| a.x.total_cmp(&b.x));
            positions
        };
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        for include_scale in [true, false] {
            h.state_mut().session.execute("keys.select", json!({"keys": []})).unwrap();
            h.run_steps(2);
            let original: Vec<_> =
                paths.iter().map(|path| h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop(path).unwrap().keys.clone()).collect();
            let positions = key_positions(&h, uids[0]);
            let rotations = key_positions(&h, uids[1]);
            let scales = key_positions(&h, uids[2]);
            click_with(&mut h, scales[0], Default::default());
            click_with(&mut h, positions[0], shift);
            click_with(&mut h, rotations[1], shift);
            if !include_scale {
                click_with(&mut h, scales[1], shift);
                click_with(&mut h, scales[0], shift);
                click_with(&mut h, scales[1], shift);
            }
            let count = if include_scale { 3 } else { 2 };
            assert_eq!(h.state().session.state.selected_keys.len(), count);
            assert_eq!(h.state().session.state.selected_props.len(), 3);
            h.input_mut().events.push(Event::Copy);
            h.step();
            let note = format!("EffectCraft: {count} keyframes");
            assert!(h.output().platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == &note)));
            h.state_mut().session.set_time(effectcraft_engine::time::Tick::from_seconds_f64(2.0));
            let undo_steps = h.state().session.history.undo.len();
            h.input_mut().events.push(Event::Paste(note));
            h.run_steps(2);
            assert_eq!(h.state().session.history.undo.len(), undo_steps + 1);
            for (path, time, value) in
                [(paths[0], if include_scale { 2.5 } else { 2.0 }, json!([100.0, 200.0, 0.0])), (paths[1], if include_scale { 3.5 } else { 3.0 }, json!(900.0))]
            {
                let p = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop(path).unwrap();
                assert_eq!(p.keys.len(), 3);
                assert_eq!(p.keys[2].time.seconds(), time);
                assert_eq!(p.keys[2].value.to_json(), value);
            }
            let scale = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop(paths[2]).unwrap();
            if include_scale {
                assert_eq!(scale.keys.len(), 3);
                assert_eq!(scale.keys[2].time.seconds(), 2.0);
                assert_eq!(scale.keys[2].value.to_json(), json!([1500.0, 1600.0, 100.0]));
            } else {
                assert_eq!(scale.keys, original[2]);
            }
            h.state_mut().session.execute("edit.undo", json!({})).unwrap();
            for (path, expected) in paths.iter().zip(&original) {
                assert_eq!(&h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop(path).unwrap().keys, expected);
            }
            h.run_steps(2);
        }
    }
}

#[test]
fn shift_click_toggles_and_ctrl_click_switches_interpolation() {
    let (mut h, id, uid) = harness();
    let ks = keys(&h, uid);
    let shift = egui::Modifiers { shift: true, ..Default::default() };
    click_with(&mut h, ks[0], Default::default());
    click_with(&mut h, ks[1], shift);
    assert_eq!(h.state().session.state.selected_keys.len(), 2, "Shift+click adds");
    click_with(&mut h, ks[1], shift);
    assert_eq!(h.state().session.state.selected_keys.len(), 1, "and takes out again");
    let key =
        |h: &Harness<'_, EffectcraftApp>| h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys[0].clone();
    // Ctrl+click: Linear → Auto Bezier → Linear.
    click_with(&mut h, ks[0], egui::Modifiers::COMMAND);
    assert!(key(&h).auto_bezier, "Auto Bezier");
    click_with(&mut h, ks[0], egui::Modifiers::COMMAND);
    assert_eq!(key(&h).out_interp, effectcraft_engine::keyframe::Interp::Linear);
    // Ctrl+Alt+click: Hold.
    click_with(&mut h, ks[0], egui::Modifiers { alt: true, ..egui::Modifiers::COMMAND });
    assert_eq!(key(&h).out_interp, effectcraft_engine::keyframe::Interp::Hold);
}

/// Clicking a key in the Timeline selects its property and layer too, as in After Effects, so
/// the Graph Editor shows the key's curve (#252).
#[test]
fn a_key_clicked_in_the_timeline_shows_in_the_graph_editor() {
    let (mut h, id, uid) = harness();
    h.state_mut().session.execute("edit.deselectAll", json!({})).unwrap();
    h.run_steps(2);
    let ks = keys(&h, uid);
    click_with(&mut h, ks[1], Default::default());
    let st = &h.state().session.state;
    assert_eq!(st.selected_keys.len(), 1);
    assert_eq!((st.selected_props.clone(), st.selected_layers.clone()), (vec![(id, uid)], vec![id]), "the key's property and layer");
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(4);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{uid}.0.1")).is_some(), "the Graph Editor shows the key");
}

#[test]
fn graph_editor_drags_move_every_selected_key_and_shift_keeps_an_axis() {
    let (mut h, id, uid) = harness();
    // Opened before selecting, so it fits all graphs (the whole comp) rather than the two keys.
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(2);
    // Fit All frames the keys closely: zoom out to the whole comp so a 0.5 s drag stays in the plot.
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.zoomFit", json!({})).unwrap();
    h.run_steps(2);
    let s = &mut h.state_mut().session;
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    h.run_steps(4);
    let at = |h: &Harness<'_, EffectcraftApp>, i: usize| {
        let e = h.state().auto.find(&format!("timeline.graph.key.{uid}.0.{i}")).unwrap_or_else(|| panic!("no graph key {i}")).clone();
        pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
    };
    let (k0, k1) = (at(&h, 0), at(&h, 1));
    let px_per_s = k1.x - k0.x;
    let values = |h: &Harness<'_, EffectcraftApp>| -> Vec<f64> {
        h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys.iter().map(|k| k.value.as_f64()).collect()
    };
    // Shift: only along time, both keys, by 0.5 s.
    drag(&mut h, k1, pos2(k1.x + px_per_s * 0.5, k1.y - 6.0), egui::Modifiers { shift: true, ..Default::default() });
    assert_eq!(key_times(&h, id), vec![0.5, 1.5], "both selected keys moved");
    assert_eq!(values(&h), vec![0.0, 100.0], "Shift kept the values");
    let undo = h.state().session.history.undo.iter().filter(|(l, _)| l == "Transform Keyframes").count();
    assert_eq!(undo, 1, "one undo step per drag");
    // Without Shift the values move too (both keys, by the same amount).
    h.run_steps(2);
    let k1 = at(&h, 1);
    drag(&mut h, k1, pos2(k1.x, k1.y + 20.0), Default::default());
    let v = values(&h);
    assert!(v[0] < 0.0 && (v[1] - 100.0 - v[0]).abs() < 1e-6, "{v:?}");
}

#[test]
fn double_click_a_key_to_edit_its_value() {
    let (mut h, id, uid) = harness();
    let k1 = keys(&h, uid)[1];
    h.input_mut().events.push(Event::PointerMoved(k1));
    h.step();
    // Both clicks inside egui's double-click time.
    for _ in 0..2 {
        h.input_mut().events.push(Event::PointerButton { pos: k1, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
        h.input_mut().events.push(Event::PointerButton { pos: k1, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    }
    h.step();
    h.run_steps(2);
    assert_eq!(h.state().dialog, Some(effectcraft_ui_egui::Dialog::Form), "the value dialog opened");
    h.run_steps(2);
    assert!(h.state().auto.find("form.field.value").is_some(), "one Opacity field");
    // OK keeps the key where it is (the dialog edits the value only).
    let ok = h.state().auto.find("form.ok").expect("form.ok").clone();
    let p = pos2(ok.rect[0] + ok.rect[2] / 2.0, ok.rect[1] + ok.rect[3] / 2.0);
    h.input_mut().events.push(Event::PointerMoved(p));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.run_steps(2);
    assert!(h.state().dialog.is_none());
    let k = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys[1].clone();
    assert_eq!((k.time.seconds(), k.value.as_f64()), (1.0, 100.0));
}

#[test]
fn k_follows_the_revealed_properties_and_auto_select_picks_the_speed_graph_for_position() {
    let (mut h, id, _) = harness();
    // A Rotation key at 2 s on a property that isn't revealed: K skips it.
    h.state_mut().session.execute("prop.addKey", json!({"layer": id.0, "path": "transform/rotation", "time": 2.0, "value": 45})).unwrap();
    h.state_mut().session.set_time(effectcraft_engine::time::Tick::from_seconds_f64(1.0));
    h.run_steps(2);
    h.input_mut().events.push(Event::Key { key: egui::Key::K, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() });
    h.run_steps(2);
    assert!(h.state().session.time().seconds() > 3.9, "past the hidden key to the work area end: {}", h.state().session.time().seconds());
    // Auto-Select Graph Type: Position shows its speed (one curve), Opacity its value.
    let s = &mut h.state_mut().session;
    for (t, v) in [(0.0, [40.0, 90.0]), (1.0, [280.0, 90.0])] {
        s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/position", "time": t, "value": v})).unwrap();
    }
    let pos = s.active_comp().unwrap().layer(id).unwrap().props.prop("transform/position").unwrap().uid;
    s.execute("prop.select", json!({"layer": id.0, "prop": pos})).unwrap();
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(4);
    assert_eq!(h.state().ui.timeline.graph_mode, "auto");
    assert!(h.state().auto.find(&format!("timeline.graph.key.{pos}.0.0")).is_some());
    assert!(h.state().auto.find(&format!("timeline.graph.key.{pos}.1.0")).is_none(), "speed graph: one curve");
    assert!(h.state().auto.find("timeline.graph.typeOptions").is_some());
    h.state_mut().ui.timeline.graph_mode = "value".into();
    h.run_steps(3);
    assert!(h.state().auto.find(&format!("timeline.graph.key.{pos}.1.0")).is_some(), "value graph: X and Y");
}

/// Dragging the handle of one selected key in the Graph Editor moves the handles of every
/// selected key with it (After Effects; Elias, 22 Oct 2026: only the grabbed one moved), in one
/// undo step: the same side of each key, or, for a key without a segment there (the last key),
/// its other side, by the same influence change. A handle at 100 % influence reaches halfway
/// across its segment, so 0.2 s on keys 1 s apart is 40 %.
#[test]
fn a_graph_handle_drag_moves_the_handles_of_every_selected_key() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/opacity", "time": 2.0, "value": 0.0})).unwrap();
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(4);
    // (in, out) influence of each key.
    let influences = |h: &Harness<'_, EffectcraftApp>| -> Vec<(f64, f64)> {
        let l = h.state().session.active_comp().unwrap().layer(id).unwrap().clone();
        let keys = l.props.prop("transform/opacity").unwrap().keys.clone();
        keys.iter().map(|k| (k.in_ease.first().map_or(0.0, |e| e.influence), k.out_ease.first().map_or(0.0, |e| e.influence))).collect()
    };
    let before = influences(&h);
    let e = h.state().auto.find(&format!("timeline.graph.handle.{uid}.0.1.out")).expect("key 1's out handle").clone();
    let k1 = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    let px_per_s = {
        let at = |i: usize| h.state().auto.find(&format!("timeline.graph.key.{uid}.0.{i}")).unwrap().rect[0];
        at(1) - at(0)
    };
    let undo = h.state().session.history.undo.len();
    // Key 1's out handle 0.2 s further out (the keys are 1 s apart).
    drag(&mut h, k1, k1 + egui::vec2(px_per_s * 0.2, 0.0), Default::default());
    let after = influences(&h);
    let grew = |a: f64, b: f64| (b - a - 0.4).abs() < 0.02;
    assert!(grew(before[1].1, after[1].1), "key 1 out: {:?} → {:?}", before[1], after[1]);
    assert!(grew(before[0].1, after[0].1), "key 0 out follows: {:?} → {:?}", before[0], after[0]);
    assert!(grew(before[2].0, after[2].0), "the last key's in follows: {:?} → {:?}", before[2], after[2]);
    assert!((after[1].0 - before[1].0).abs() < 1e-9, "key 1's in handle stays");
    assert_eq!(h.state().session.history.undo.len(), undo + 1, "one undo step");
    // A key that isn't selected keeps its handles when another key's handle is dragged alone.
    h.state_mut().session.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": 1.0}]})).unwrap();
    h.run_steps(3);
    let before = influences(&h);
    let e = h.state().auto.find(&format!("timeline.graph.handle.{uid}.0.1.out")).expect("key 1's out handle").clone();
    let k1 = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    drag(&mut h, k1, k1 - egui::vec2(px_per_s * 0.1, 0.0), Default::default());
    let after = influences(&h);
    assert!(after[1].1 < before[1].1 - 0.1, "key 1 out moved: {:?} → {:?}", before[1], after[1]);
    assert_eq!((after[0], after[2]), (before[0], before[2]), "unselected keys stay");
}

/// The Timeline's speed rows (Elias, 22 Oct 2026): a handle drags up for speed (velocity) and
/// sideways for influence, right next to the keys. The segment's other key stays, even with both
/// keys selected (Elias, 23 Oct 2026: its handle moved with it, mirrored); other properties'
/// selected keys follow (engine test `a_handle_drag_eases_every_selected_key`), and with ⇧ none do.
#[test]
fn a_speed_row_handle_drag_leaves_the_segments_other_key() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    h.run_steps(4);
    let e = h.state().auto.find(&format!("timeline.speed.{uid}.0.out")).expect("key 0's out handle in the row").clone();
    let from = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    let undo = h.state().session.history.undo.len();
    drag(&mut h, from, from + egui::vec2(30.0, -6.0), Default::default());
    let keys = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys.clone();
    let (out0, in1) = (keys[0].out_ease.first().copied().unwrap(), keys[1].in_ease.first().copied().unwrap());
    assert!(out0.influence > 0.4, "influence grew: {out0:?}");
    assert!(out0.speed > 1.0, "speed rose: {out0:?}");
    assert!((in1.influence - 1.0 / 3.0).abs() < 1e-6 && in1.speed.abs() < 1e-6, "key 1 stays: {in1:?}");
    assert_eq!(h.state().session.history.undo.len(), undo + 1, "one undo step");
    h.run_steps(3);
    let e = h.state().auto.find(&format!("timeline.speed.{uid}.0.out")).expect("key 0's out handle in the row").clone();
    let from = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    drag(&mut h, from, from + egui::vec2(10.0, -4.0), egui::Modifiers::SHIFT);
    let keys = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys.clone();
    let (out0_after, in1) = (keys[0].out_ease.first().copied().unwrap(), keys[1].in_ease.first().copied().unwrap());
    assert!(out0_after.influence > out0.influence, "⇧-drag moved key 0's out: {out0:?} → {out0_after:?}");
    assert!((in1.influence - 1.0 / 3.0).abs() < 1e-6 && in1.speed.abs() < 1e-6, "with ⇧ key 1 stays too: {in1:?}");
}

/// Elias, 23 Oct 2026: a speed-row drag left its undo merge open on release, so the next
/// handle drag started from before it and put the first handle back (his 98 % in handle fell to
/// 16.67 % the moment he touched the other key's out handle). Each drag is its own step now.
#[test]
fn a_second_speed_row_drag_keeps_the_first_one() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    s.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": 1.0}]})).unwrap();
    h.run_steps(4);
    let centre = |h: &Harness<'_, EffectcraftApp>, name: &str| {
        let e = h.state().auto.find(&format!("timeline.speed.{uid}.{name}")).unwrap_or_else(|| panic!("{name} handle in the row")).clone();
        pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
    };
    let eases = |h: &Harness<'_, EffectcraftApp>| {
        let keys = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys.clone();
        (keys[0].out_ease.first().copied().unwrap(), keys[1].in_ease.first().copied().unwrap())
    };
    let undo = h.state().session.history.undo.len();
    // Key 1's in handle out to the left (more influence), then key 0's out handle a little.
    let from = centre(&h, "1.in");
    drag(&mut h, from, from - egui::vec2(40.0, 0.0), Default::default());
    let (_, in1) = eases(&h);
    assert!(in1.influence > 1.0 / 3.0 + 0.02, "key 1 in grew: {in1:?}");
    h.run_steps(3);
    let from = centre(&h, "0.out");
    drag(&mut h, from, from + egui::vec2(10.0, 0.0), Default::default());
    let (out0, in1_after) = eases(&h);
    assert!(out0.influence > 1.0 / 3.0, "key 0 out moved: {out0:?}");
    assert!((in1_after.influence - in1.influence).abs() < 1e-9, "key 1 in kept: {in1:?} → {in1_after:?}");
    assert_eq!(h.state().session.history.undo.len(), undo + 2, "two drags, two undo steps");
}

/// A handle at speed 0 (Easy Ease) dragged up in the speed graph speeds the value the way its
/// segment goes: on a falling segment the speed is negative. It came out positive, so the value
/// overshot, stopped and turned back just before the key.
#[test]
fn a_zero_speed_handle_dragged_in_the_speed_graph_keeps_its_segments_direction() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("prop.addKey", json!({"layer": id.0, "path": "transform/opacity", "time": 2.0, "value": 0.0})).unwrap();
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    h.state_mut().ui.timeline.graph_editor = true;
    h.state_mut().ui.timeline.graph_mode = "speed".into();
    h.run_steps(4);
    // Key 1's handle on the falling segment, then key 0's (every selected key follows each).
    for key in [1, 0] {
        let e = h.state().auto.find(&format!("timeline.graph.handle.{uid}.0.{key}.out")).unwrap_or_else(|| panic!("key {key}'s out handle")).clone();
        let p = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
        drag(&mut h, p, p - egui::vec2(0.0, 60.0), Default::default());
    }
    let keys = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys.clone();
    let out = |i: usize| keys[i].out_ease.first().map_or(0.0, |e| e.speed);
    assert!(out(1) < 0.0, "100 → 0 falls: {}", out(1));
    assert!(out(0) > 0.0, "0 → 100 rises: {}", out(0));
}

/// Whether the key mark `id` was drawn selected in the last frame (its automation label says so).
fn drawn_selected(h: &Harness<'_, EffectcraftApp>, id: &str) -> bool {
    let a = &h.state().auto;
    let e = a.elements.iter().chain(a.previous.iter()).find(|e| e.id == id).unwrap_or_else(|| panic!("no element {id}"));
    e.label.ends_with("(selected)")
}

/// The property's key mark ids, left to right.
fn key_ids(h: &Harness<'_, EffectcraftApp>, uid: u64) -> Vec<String> {
    let mut v: Vec<(f32, String)> = h.state().auto.query(&format!("timeline.key.{uid}.")).iter().map(|e| (e.rect[0], e.id.clone())).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v.into_iter().map(|(_, id)| id).collect()
}

fn press(h: &mut Harness<'_, EffectcraftApp>, p: Pos2, modifiers: egui::Modifiers) {
    h.input_mut().events.push(Event::PointerMoved(p));
    h.step();
    h.input_mut().events.push(Event::ModifiersChanged(modifiers));
    h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers });
    h.step();
}

/// Move with the button held, in small steps.
fn move_to(h: &mut Harness<'_, EffectcraftApp>, from: Pos2, to: Pos2) {
    for i in 1..=8 {
        h.input_mut().events.push(Event::PointerMoved(from + (to - from) * (i as f32 / 8.0)));
        h.step();
    }
}

fn release(h: &mut Harness<'_, EffectcraftApp>, p: Pos2, modifiers: egui::Modifiers) {
    h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers });
    h.step();
    h.input_mut().events.push(Event::ModifiersChanged(Default::default()));
    h.run_steps(2);
}

/// A key marquee shows the keys it would select while it is dragged, Shift adding them to the
/// selection; leaving the box un-highlights them; the release commits what was shown (After
/// Effects highlights live; keys used to turn selected only on release).
#[test]
fn a_key_marquee_highlights_keys_while_the_button_is_down() {
    let (mut h, _, uid) = harness();
    h.state_mut().session.execute("keys.select", json!({"keys": []})).unwrap();
    h.run_steps(2);
    let ks = keys(&h, uid);
    let ids = key_ids(&h, uid);
    // From between the keys (empty time graph), over the 1 s key.
    let start = pos2((ks[0].x + ks[1].x) / 2.0, ks[1].y - 6.0);
    let over = pos2(ks[1].x + 20.0, ks[1].y + 6.0);
    press(&mut h, start, Default::default());
    move_to(&mut h, start, over);
    assert!(drawn_selected(&h, &ids[1]), "the key in the box shows selected while dragging");
    assert!(!drawn_selected(&h, &ids[0]));
    assert!(h.state().session.state.selected_keys.is_empty(), "not committed before the release");
    // Back out of it: un-highlighted, live.
    let back = pos2(ks[1].x - 20.0, ks[1].y + 6.0);
    move_to(&mut h, over, back);
    assert!(!drawn_selected(&h, &ids[1]), "leaving the box un-highlights it");
    move_to(&mut h, back, over);
    assert!(drawn_selected(&h, &ids[1]));
    release(&mut h, over, Default::default());
    assert_eq!(h.state().session.state.selected_keys.len(), 1, "the release commits what was shown");
    assert!(drawn_selected(&h, &ids[1]) && !drawn_selected(&h, &ids[0]));

    // Shift: the box adds to the selection, shown live too.
    let shift = egui::Modifiers { shift: true, ..Default::default() };
    let start = pos2(ks[0].x + 15.0, ks[0].y - 6.0);
    let over = pos2(ks[0].x - 8.0, ks[0].y + 6.0);
    press(&mut h, start, shift);
    move_to(&mut h, start, over);
    assert!(drawn_selected(&h, &ids[0]) && drawn_selected(&h, &ids[1]), "Shift shows the keys added to the selection");
    release(&mut h, over, shift);
    assert_eq!(h.state().session.state.selected_keys.len(), 2);
}

/// Pressing a key selects it on the press, as in After Effects; pressing one of several selected
/// keys keeps them all (a drag moves them all) and a release without a drag reduces the
/// selection to that key.
#[test]
fn a_key_turns_selected_on_the_press() {
    let (mut h, id, uid) = harness();
    h.state_mut().session.execute("keys.select", json!({"keys": []})).unwrap();
    h.run_steps(2);
    let ks = keys(&h, uid);
    let ids = key_ids(&h, uid);
    press(&mut h, ks[1], Default::default());
    h.run_steps(1);
    assert_eq!(h.state().session.state.selected_keys.len(), 1, "selected while the button is down");
    assert!(drawn_selected(&h, &ids[1]));
    release(&mut h, ks[1], Default::default());
    assert_eq!(h.state().session.state.selected_keys.len(), 1);

    // Two selected: pressing one keeps both; releasing without a drag reduces to it.
    h.state_mut().session.execute("keys.selectAll", json!({})).unwrap();
    h.run_steps(2);
    press(&mut h, ks[0], Default::default());
    h.run_steps(1);
    assert_eq!(h.state().session.state.selected_keys.len(), 2, "the group stays selected on the press");
    release(&mut h, ks[0], Default::default());
    assert_eq!(h.state().session.state.selected_keys.len(), 1, "a click reduces the selection to the key");

    // Two selected: pressing one and dragging moves both.
    h.state_mut().session.execute("keys.selectAll", json!({})).unwrap();
    h.run_steps(2);
    let px_per_s = ks[1].x - ks[0].x;
    drag(&mut h, ks[0], pos2(ks[0].x + px_per_s * 0.5, ks[0].y), Default::default());
    assert_eq!(key_times(&h, id), vec![0.5, 1.5], "both selected keys moved");
    assert_eq!(h.state().session.state.selected_keys.len(), 2);
}

/// The Graph Editor's key marquee highlights live too.
#[test]
fn a_graph_editor_marquee_highlights_keys_while_the_button_is_down() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.select", json!({"keys": []})).unwrap();
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(4);
    let key = |i: usize| format!("timeline.graph.key.{uid}.0.{i}");
    let e = h.state().auto.find(&key(1)).unwrap().clone();
    let k1 = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    let start = pos2(k1.x - 30.0, k1.y + 30.0);
    let over = pos2(k1.x + 12.0, k1.y - 8.0);
    press(&mut h, start, Default::default());
    move_to(&mut h, start, over);
    assert!(drawn_selected(&h, &key(1)), "the key in the box shows selected while dragging");
    assert!(!drawn_selected(&h, &key(0)));
    assert!(h.state().session.state.selected_keys.is_empty(), "not committed before the release");
    release(&mut h, over, Default::default());
    assert_eq!(h.state().session.state.selected_keys.len(), 1);
}

/// #457: on a Timeline narrower than the Graph Editor's whole button row, the buttons that don't
/// fit go to a "»" menu at the end of the bar instead of being cut off; each stays reachable and
/// works from there.
#[test]
fn graph_editor_buttons_that_do_not_fit_move_to_the_more_menu() {
    let (mut h, _, _) = harness();
    h.set_size(egui::vec2(1000.0, 800.0));
    h.state_mut().ui.timeline.outline_w = Some(740.0);
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(4);
    let ids = [
        "typeOptions",
        "propertyOptions",
        "autoZoom",
        "fit",
        "fitSelection",
        "snap",
        "transformBox",
        "separateDimensions",
        "interpolationHold",
        "interpolationLinear",
        "interpolationAuto",
        "easyEaseEasy",
        "easyEaseInEase",
        "easyEaseOutEase",
    ];
    let graph = h.state().auto.find("timeline.graph").expect("the Graph Editor is shown").rect;
    let right = graph[0] + graph[2];
    let shown: Vec<&str> = ids.iter().copied().filter(|id| h.state().auto.find(&format!("timeline.graph.{id}")).is_some()).collect();
    for id in &shown {
        let r = h.state().auto.find(&format!("timeline.graph.{id}")).unwrap().rect;
        assert!(r[0] + r[2] <= right, "{id} is drawn past the bar's right edge");
    }
    let more = h.state().auto.find("timeline.graph.more").expect("the buttons that don't fit are in a menu").rect;
    assert!(more[0] + more[2] <= right);
    let p = pos2(more[0] + more[2] / 2.0, more[1] + more[3] / 2.0);
    let click = |h: &mut Harness<'_, EffectcraftApp>, p: Pos2| {
        h.input_mut().events.push(Event::PointerMoved(p));
        h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
        h.step();
        h.input_mut().events.push(Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
        h.run_steps(3);
    };
    click(&mut h, p);
    let hidden: Vec<&str> = ids.iter().copied().filter(|id| h.state().auto.find(&format!("timeline.graph.more.{id}")).is_some()).collect();
    assert_eq!(shown.len() + hidden.len(), ids.len(), "every button is on the bar or in the menu: {shown:?} + {hidden:?}");
    assert!(hidden.contains(&"transformBox"), "{hidden:?}");
    let before = h.state().ui.timeline.graph_transform_box;
    let item = h.state().auto.find("timeline.graph.more.transformBox").unwrap().rect;
    click(&mut h, pos2(item[0] + item[2] / 2.0, item[1] + item[3] / 2.0));
    assert_eq!(h.state().ui.timeline.graph_transform_box, !before, "the menu item runs the button");
}

/// #355, #491: with three layers selected the Properties panel heads "(3) Selected Objects"; its
/// stopwatch animates Position on all three (one undo step) and scrubbing X moves all three by
/// the same amount.
#[test]
fn properties_panel_edits_every_selected_layer() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Three", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    let mut ids = vec![];
    for (i, c) in ["#e04020", "#20e040", "#2040e0"].iter().enumerate() {
        let l = s.execute("layer.newSolid", json!({"color": c, "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
        s.execute("prop.set", json!({"layer": l, "path": "transform/position", "value": [60.0 + 80.0 * i as f64, 90.0]})).unwrap();
        ids.push(l);
    }
    s.execute("layer.select", json!({"layers": ids})).unwrap();
    let first = s.state.selected_layers[0];
    let uid = s.active_comp().unwrap().layer(first).unwrap().props.prop("transform/position").unwrap().uid;
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.state_mut().show_panel(effectcraft_ui_egui::dock::PanelKind::Properties);
    h.run_steps(4);
    assert_eq!(h.state().auto.find("properties.selectedObjects").map(|e| e.label.clone()), Some("(3) Selected Objects".into()));
    let pos = |h: &Harness<'_, EffectcraftApp>, l: u64| {
        h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap().clone()
    };
    let at = |h: &Harness<'_, EffectcraftApp>, id: &str| {
        let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no {id}"));
        pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
    };
    let sw = at(&h, &format!("properties.prop.{uid}.stopwatch"));
    h.input_mut().events.push(Event::PointerMoved(sw));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: sw, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: sw, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.run_steps(3);
    assert!(ids.iter().all(|l| pos(&h, *l).is_animated()), "every selected layer's stopwatch");
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    h.run_steps(2);
    assert!(ids.iter().all(|l| !pos(&h, *l).is_animated()), "one undo step");
    // Scrub X: each layer moves by the same amount.
    let before: Vec<f64> = ids.iter().map(|l| pos(&h, *l).value.components()[0]).collect();
    let x = at(&h, &format!("properties.prop.{uid}.value.0"));
    drag(&mut h, x, x + egui::vec2(30.0, 0.0), Default::default());
    let after: Vec<f64> = ids.iter().map(|l| pos(&h, *l).value.components()[0]).collect();
    let d = after[0] - before[0];
    assert!(d.abs() > 1.0, "the scrub moved X: {before:?} → {after:?}");
    for (a, b) in after.iter().zip(&before) {
        assert!((a - b - d).abs() < 1e-6, "same change on every layer: {before:?} → {after:?}");
    }
}

/// #770: Proportional Scrubbing (After Effects 26.2). With three layers selected, a Ctrl+Alt
/// (Cmd+Opt) scrub of Position X in the Timeline leaves the first selected layer where it is,
/// moves the last by the whole change and the middle one by half, whichever layer is scrubbed.
#[test]
fn ctrl_alt_scrub_in_the_timeline_is_proportional() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Three", "width": 320, "height": 180, "frameRate": 30, "duration": 4})).unwrap();
    let mut ids = vec![];
    for c in ["#e04020", "#20e040", "#2040e0"] {
        let l = s.execute("layer.newSolid", json!({"color": c, "width": 40, "height": 40})).unwrap()["layer"].as_u64().unwrap();
        s.execute("prop.set", json!({"layer": l, "path": "transform/position", "value": [100.0, 90.0]})).unwrap();
        ids.push(l);
    }
    // Selection order: the first created, then the second, then the third.
    for (i, l) in ids.iter().enumerate() {
        s.execute("layer.select", json!({"layers": [l], "add": i > 0})).unwrap();
    }
    let middle = ids[1];
    let uid = s.active_comp().unwrap().layer(LayerId(middle)).unwrap().props.prop("transform/position").unwrap().uid;
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(3);
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.reveal.position", json!({})).unwrap();
    h.run_steps(4);
    let x = |h: &Harness<'_, EffectcraftApp>, l: u64| {
        h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap().value.components()[0]
    };
    let e = h.state().auto.find(&format!("timeline.prop.{uid}.value.0")).cloned().expect("the middle layer's Position X shows");
    let from = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    let ctrl_alt = egui::Modifiers { alt: true, ctrl: true, command: true, ..Default::default() };
    drag(&mut h, from, from + egui::vec2(40.0, 0.0), ctrl_alt);
    let moved: Vec<f64> = ids.iter().map(|l| x(&h, *l) - 100.0).collect();
    assert!(moved[2] > 1.0, "the last selected layer moved: {moved:?}");
    assert!(moved[0].abs() < 1e-9, "the first selected layer stays: {moved:?}");
    assert!((moved[1] - moved[2] / 2.0).abs() < 1e-6, "the middle one moves by half: {moved:?}");
}

/// #1171: in the speed graph an end key sits at its one side's speed, so it rises with the handle
/// being dragged; once it left the plot its handles were skipped and the drag stopped at the top
/// edge. A drag that ends above the graph keeps raising the speed as far as the pointer goes.
#[test]
fn speed_graph_handle_drags_upward_past_the_top() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    h.state_mut().ui.timeline.graph_editor = true;
    h.state_mut().ui.timeline.graph_mode = "speed".into();
    h.run_steps(4);
    let out_speed = |h: &Harness<'_, EffectcraftApp>| {
        h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys[0].out_ease[0].speed
    };
    let e = h.state().auto.find(&format!("timeline.graph.handle.{uid}.0.0.out")).cloned().expect("the first key's out handle shows");
    let at = pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0);
    drag(&mut h, at, at + egui::vec2(0.0, -40.0), Default::default());
    let short = out_speed(&h);
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    h.run_steps(4);
    assert_eq!(out_speed(&h), 0.0);
    drag(&mut h, at, at + egui::vec2(0.0, -400.0), Default::default());
    let long = out_speed(&h);
    // The speed per pixel depends on the graph's fitted scale; only the proportion matters.
    assert!(short > 0.0 && long > 9.0 * short, "40 px up gave {short}, 400 px up gave {long}");
}

#[test]
fn option_drag_splits_and_rejoins_speed_handles_with_one_undo_each() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("prop.addKey", json!({"layer": id.0, "prop": uid, "time": 2.0, "value": 200.0})).unwrap();
    s.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": 1.0}]})).unwrap();
    s.execute("keys.setEase", json!({"layer": id.0, "prop": uid, "time": 1.0, "side": "out", "speed": 100.0, "influence": 30.0, "continuous": true})).unwrap();
    h.state_mut().ui.timeline.graph_editor = true;
    h.state_mut().ui.timeline.graph_mode = "speed".into();
    h.run_steps(4);
    h.state_mut().ui.timeline.graph_auto_zoom = false;
    h.state_mut().ui.timeline.graph_range = Some((0.0, 300.0));
    h.run_steps(3);
    let center = |h: &Harness<'_, EffectcraftApp>| {
        let r = h.state().auto.find(&format!("timeline.graph.handle.{uid}.0.1.out")).unwrap().rect;
        pos2(r[0] + r[2] * 0.5, r[1] + r[3] * 0.5)
    };
    let key = |h: &Harness<'_, EffectcraftApp>| h.state().session.active_comp().unwrap().layer(id).unwrap().props.find(uid).unwrap().keys[1].clone();
    let start = center(&h);
    let undo = h.state().session.history.undo.len();
    drag(&mut h, start, start - egui::vec2(0.0, 30.0), egui::Modifiers::ALT);
    assert!(!key(&h).continuous);
    assert_eq!(key(&h).in_ease[0].speed, 100.0);
    assert!(key(&h).out_ease[0].speed > 100.0);
    assert_eq!(h.state().session.history.undo.len(), undo + 1);
    let start = center(&h);
    let plot = h.state().auto.find("timeline.graph").unwrap().rect;
    let delta = ((key(&h).out_ease[0].speed - 100.0) / 300.0) * f64::from(plot[3]);
    // Option at the press on split handles joins them (decided once, not by how close they get).
    drag(&mut h, start, start + egui::vec2(0.0, delta as f32), egui::Modifiers::ALT);
    assert!(key(&h).continuous);
    assert!((key(&h).out_ease[0].speed - key(&h).in_ease[0].speed).abs() < 1e-9);
    assert_eq!(h.state().session.history.undo.len(), undo + 2);
}

#[test]
fn graph_value_snapping_can_be_temporarily_disabled() {
    for disable in [false, true] {
        let (mut h, id, uid) = harness();
        h.state_mut().session.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": 0.0}]})).unwrap();
        h.state_mut().ui.timeline.graph_editor = true;
        h.run_steps(4);
        h.state_mut().ui.timeline.graph_auto_zoom = false;
        h.state_mut().ui.timeline.graph_range = Some((-20.0, 150.0));
        h.state_mut().ui.timeline.graph_transform_box = false;
        h.run_steps(3);
        let center = |index| {
            let r = h.state().auto.find(&format!("timeline.graph.key.{uid}.0.{index}")).unwrap().rect;
            pos2(r[0] + r[2] * 0.5, r[1] + r[3] * 0.5)
        };
        let (from, other) = (center(0), center(1));
        drag(&mut h, from, pos2(from.x, other.y + 2.0), if disable { egui::Modifiers::COMMAND } else { egui::Modifiers::SHIFT });
        let k = &h.state().session.active_comp().unwrap().layer(id).unwrap().props.find(uid).unwrap().keys[0];
        let value = k.value.components()[0];
        if disable {
            assert!((value - 100.0).abs() > 0.1, "Command disables snapping: {value}");
        } else {
            assert!((value - 100.0).abs() < 1e-6, "snapped to other key value: {value}");
        }
        assert_eq!(k.time.seconds(), 0.0);
    }
}

/// #1617: with Easy Ease both keys sit at speed 0 in the speed graph, so fitting the selection to
/// its keys and handles framed -1..1 while the curve between them peaks far above. Opening the
/// Graph Editor on selected keys and Fit Selection both frame the curve's peak, and a lone
/// selected key frames its neighbouring segments.
#[test]
fn speed_graph_fit_selection_frames_the_curve_between_the_keys() {
    let (mut h, id, uid) = harness();
    let s = &mut h.state_mut().session;
    s.execute("prop.select", json!({"layer": id.0, "prop": uid})).unwrap();
    s.execute("keys.selectAll", json!({})).unwrap();
    s.execute("keys.easyEase", json!({})).unwrap();
    let keys = h.state().session.active_comp().unwrap().layer(id).unwrap().props.prop("transform/opacity").unwrap().keys.clone();
    let peak = (0..=100)
        .map(|i| effectcraft_engine::keyframe::velocity(&keys, effectcraft_engine::time::Tick::from_seconds_f64(i as f64 / 100.0), false)[0].abs())
        .fold(0.0, f64::max);
    assert!(peak > 100.0, "the eased curve peaks above its average speed: {peak}");
    h.state_mut().ui.timeline.graph_mode = "speed".into();
    h.state_mut().ui.timeline.graph_editor = true;
    h.run_steps(4);
    let frames_peak = |h: &Harness<'_, EffectcraftApp>, what: &str| {
        let (lo, hi) = h.state().ui.timeline.graph_range.expect("a fitted range");
        assert!(lo <= 0.0 && hi > peak * 0.9, "{what}: range {lo}..{hi}, the curve peaks at {peak}");
    };
    frames_peak(&h, "on open");
    h.state_mut().ui.timeline.graph_range = Some((-1.0, 1.0));
    let fit = h.state().auto.find("timeline.graph.fitSelection").expect("the Fit Selection button").rect;
    click_with(&mut h, pos2(fit[0] + fit[2] / 2.0, fit[1] + fit[3] / 2.0), Default::default());
    frames_peak(&h, "Fit Selection");
    let last = keys[1].time.seconds();
    h.state_mut().session.execute("keys.select", json!({"keys": [{"layer": id.0, "prop": uid, "time": last}]})).unwrap();
    assert_eq!(h.state().session.state.selected_keys.len(), 1);
    h.state_mut().ui.timeline.graph_range = Some((-1.0, 1.0));
    click_with(&mut h, pos2(fit[0] + fit[2] / 2.0, fit[1] + fit[3] / 2.0), Default::default());
    frames_peak(&h, "lone key");
}
