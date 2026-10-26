//! Timeline rows for shape layers, masks and keyframing: the Contents "Add:" menu and the Classic
//! 3D "Change Renderer…" row (#206), Alt+Shift+P revealing the key it adds and clicks below the
//! layers deselecting them (#205), Mask Feather's linked values (#203), and copying shape items
//! between shape layers (#227), Ctrl+V of SVG markup making a shape layer (#738), and Cmd+C /
//! Cmd+V of a selected effect onto another shape layer.

use effectcraft_engine::Session;
use effectcraft_engine::project::LayerId;
use effectcraft_ui_egui::{Dialog, EffectcraftApp};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, pos2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

fn harness(s: Session) -> Harness<'static, EffectcraftApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(3);
    h
}

fn session() -> Session {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Main", "width": 320, "height": 180, "duration": 4})).unwrap();
    s
}

fn center(h: &Harness<'_, EffectcraftApp>, id: &str) -> Pos2 {
    let e = h.state().auto.find(id).unwrap_or_else(|| panic!("no {id}"));
    pos2(e.rect[0] + e.rect[2] / 2.0, e.rect[1] + e.rect[3] / 2.0)
}

/// Step until the viewer's frame has rendered (frames render on background threads).
fn settle(h: &mut Harness<'_, EffectcraftApp>) {
    for _ in 0..600 {
        h.step();
        if h.state().frames.inflight() == 0 && h.state().frames.last_ms.lock().map(|v| *v > 0.0).unwrap_or(false) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(2);
}

fn click_at(h: &mut Harness<'_, EffectcraftApp>, p: Pos2) {
    h.input_mut().events.push(Event::PointerMoved(p));
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE });
    h.step();
    h.input_mut().events.push(Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
}

fn click(h: &mut Harness<'_, EffectcraftApp>, id: &str) {
    let p = center(h, id);
    click_at(h, p);
}

fn open_layer(h: &mut Harness<'_, EffectcraftApp>, id: u64) {
    h.state_mut().ui.timeline.open_layers.insert(id);
    h.run_steps(2);
}

#[test]
fn shape_contents_add_menu_adds_path_operations() {
    let mut s = session();
    let l = s.execute("layer.newShape", json!({"kind": "ellipse"})).unwrap()["layer"].as_u64().unwrap();
    let contents = s.active_comp().unwrap().layer(LayerId(l)).unwrap().props.sub("contents").unwrap().uid;
    let mut h = harness(s);
    open_layer(&mut h, l);
    click(&mut h, &format!("timeline.group.{contents}.add"));
    // The list scrolls past about 18 rows: scroll it to the end to reach Trim Paths.
    let at = h.get_by_label("Rectangle").rect().center();
    h.input_mut().events.push(Event::PointerMoved(at));
    h.input_mut().events.push(Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -1000.0),
        modifiers: Default::default(),
        phase: egui::TouchPhase::Move,
    });
    h.run_steps(5);
    let entry = h.get_by_label("Trim Paths").rect();
    assert!(entry.max.y <= 1000.0, "the entry is inside the window: {entry:?}");
    click_at(&mut h, entry.center());
    let layer = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().clone();
    let items: Vec<&str> = layer.props.sub("contents").unwrap().groups().map(|g| g.match_id.as_str()).collect();
    assert_eq!(items, ["group", "trim"]);
}

#[test]
fn classic_3d_shape_layers_offer_change_renderer() {
    let mut s = session();
    let l = s.execute("layer.newShape", json!({"kind": "rect"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.setSwitch", json!({"layers": [l], "switch": "threeD", "value": true})).unwrap();
    let mut h = harness(s);
    open_layer(&mut h, l);
    click(&mut h, &format!("timeline.layer.{l}.changeRenderer"));
    assert_eq!(h.state().dialog, Some(Dialog::CompSettings));
    assert!(h.state().auto.find("dialog.comp.renderer").is_some(), "on the 3D Renderer tab");
    // Advanced 3D shows the extrusion options instead.
    h.state_mut().dialog = None;
    h.state_mut().session.execute("comp.renderer", json!({"renderer": "advanced3d"})).unwrap();
    h.run_steps(3);
    assert!(h.state().auto.find(&format!("timeline.layer.{l}.changeRenderer")).is_none());
}

#[test]
fn add_position_key_reveals_position() {
    let mut s = session();
    let l = s.execute("layer.newSolid", json!({"color": "#808080"})).unwrap()["layer"].as_u64().unwrap();
    let mut h = harness(s);
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "timeline.keyAt.position", json!({})).unwrap();
    h.run_steps(2);
    let app = h.state();
    let position = app.session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop("transform/position").unwrap().clone();
    assert_eq!(position.keys.len(), 1);
    assert_eq!(app.ui.timeline.layer_reveal.get(&l), Some(&vec!["position".to_string()]));
    assert!(app.auto.find(&format!("timeline.prop.{}.value.0", position.uid)).is_some(), "the Position row shows");
}

#[test]
fn clicking_below_the_layers_deselects_them() {
    let mut s = session();
    let l = s.execute("layer.newSolid", json!({"color": "#808080"})).unwrap()["layer"].as_u64().unwrap();
    let mut h = harness(s);
    assert_eq!(h.state().session.state.selected_layers, vec![LayerId(l)]);
    // In the time graph, below the only layer.
    let bar = center(&h, &format!("timeline.layer.{l}.bar"));
    click_at(&mut h, bar + egui::vec2(0.0, 80.0));
    assert!(h.state().session.state.selected_layers.is_empty());
}

#[test]
fn mask_feather_values_are_linked_and_not_negative() {
    let mut s = session();
    let l = s.execute("layer.newSolid", json!({"color": "#808080"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.addMask", json!({"layer": l, "shape": "rect"})).unwrap();
    let feather = |h: &Harness<'_, EffectcraftApp>| {
        let p = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.prop("masks/#1/feather").unwrap().clone();
        (p.uid, p.value.components())
    };
    let mut h = harness(s);
    h.state_mut().ui.timeline.open_layers.insert(l);
    h.state_mut().ui.timeline.layer_reveal.insert(l, vec!["feather".into()]);
    h.run_steps(2);
    let (uid, _) = feather(&h);
    let type_value = |h: &mut Harness<'_, EffectcraftApp>, d: usize, v: &str| {
        click(h, &format!("timeline.prop.{uid}.value.{d}"));
        h.input_mut().events.push(Event::Text(v.into()));
        h.step();
        h.input_mut().events.push(Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
        h.run_steps(3);
    };
    type_value(&mut h, 0, "12");
    assert_eq!(feather(&h).1, vec![12.0, 12.0], "linked by default");
    type_value(&mut h, 1, "-5");
    assert_eq!(feather(&h).1, vec![0.0, 0.0], "never negative");
    click(&mut h, &format!("timeline.prop.{uid}.link"));
    type_value(&mut h, 1, "7");
    assert_eq!(feather(&h).1, vec![0.0, 7.0], "unlinked");
}

/// Two shape layers, "Rect" (Rectangle 1) and "Oval" (Ellipse 1, selected): (harness with Rect's
/// Contents open, Rect, Oval, Rectangle 1's uid).
fn two_shape_layers() -> (Harness<'static, EffectcraftApp>, u64, u64, u64) {
    let mut s = session();
    let a = s.execute("layer.newShape", json!({"kind": "rect", "name": "Rect"})).unwrap()["layer"].as_u64().unwrap();
    let b = s.execute("layer.newShape", json!({"kind": "ellipse", "name": "Oval"})).unwrap()["layer"].as_u64().unwrap();
    let contents = s.active_comp().unwrap().layer(LayerId(a)).unwrap().props.sub("contents").unwrap().clone();
    let rect = contents.groups().next().unwrap().uid;
    let mut h = harness(s);
    h.state_mut().ui.timeline.open_groups.insert(contents.uid);
    open_layer(&mut h, a);
    (h, a, b, rect)
}

fn contents_names(h: &Harness<'_, EffectcraftApp>, l: u64) -> Vec<String> {
    h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.sub("contents").unwrap().groups().map(|g| g.name.clone()).collect()
}

#[test]
fn ctrl_c_and_ctrl_v_copy_a_shape_group_into_another_shape_layer() {
    let (mut h, a, b, rect) = two_shape_layers();
    click(&mut h, &format!("timeline.group.{rect}.name"));
    assert_eq!(h.state().session.state.selected_props, vec![(LayerId(a), rect)]);
    h.input_mut().events.push(Event::Copy);
    h.step();
    click(&mut h, &format!("timeline.layer.{b}.row"));
    assert_eq!(h.state().session.state.selected_layers, vec![LayerId(b)]);
    h.input_mut().events.push(Event::Paste("EffectCraft: 1 shape item".into()));
    h.run_steps(3);
    assert_eq!(h.state().session.active_comp().unwrap().layers.len(), 2, "no new layer");
    assert_eq!(contents_names(&h, b), ["Rectangle 1", "Ellipse 1"]);
    assert_eq!(contents_names(&h, a), ["Rectangle 1"]);
}

#[test]
fn ctrl_d_duplicates_the_selected_shape_group_in_its_layer() {
    let (mut h, a, _, rect) = two_shape_layers();
    click(&mut h, &format!("timeline.group.{rect}.name"));
    h.input_mut().events.push(Event::Key { key: Key::D, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND });
    h.run_steps(3);
    assert_eq!(h.state().session.active_comp().unwrap().layers.len(), 2, "no new layer");
    assert_eq!(contents_names(&h, a), ["Rectangle 2", "Rectangle 1"]);
}

#[test]
fn toolbar_stroke_edits_the_selected_shape_layer() {
    let mut s = session();
    let l = s.execute("layer.newShape", json!({"kind": "rect", "fill": "#ff0000"})).unwrap()["layer"].as_u64().unwrap();
    let mut h = harness(s);
    // The Selection tool is active: the options show for the selected shape layer.
    click(&mut h, "header.strokeWidth");
    h.input_mut().events.push(Event::Text("4".into()));
    h.step();
    h.input_mut().events.push(Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(3);
    let layer = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().clone();
    let width = layer.props.prop("contents/group/contents/stroke/width").map(|p| p.value.as_f64());
    assert_eq!(width, Some(4.0), "a stroke was added with the toolbar's width");
    assert_eq!(h.state().session.state.shape_tool.stroke_width, 4.0);
}

/// The ⇄ Reverse Path Direction button on a shape item's row (After Effects): each click
/// toggles the item's Path Direction as one undo step; the direction itself isn't a row.
#[test]
fn reverse_path_direction_button_toggles_the_path_direction() {
    let mut s = session();
    let l = s.execute("layer.newShape", json!({"kind": "ellipse"})).unwrap()["layer"].as_u64().unwrap();
    let (contents, group, ellipse, fill) = {
        let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap();
        let g = layer.props.group("contents/group").unwrap();
        let c = g.sub("contents").unwrap();
        (vec![layer.props.sub("contents").unwrap().uid, c.uid], g.uid, c.sub("ellipse").unwrap().uid, c.sub("fill").unwrap().uid)
    };
    let mut h = harness(s);
    open_layer(&mut h, l);
    h.state_mut().ui.timeline.open_groups.extend(contents.into_iter().chain([group, ellipse]));
    h.run_steps(2);
    let dir = |h: &Harness<'_, EffectcraftApp>| {
        let layer = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().clone();
        layer.props.find_group(ellipse).unwrap().get("direction").unwrap().value.clone()
    };
    let button = format!("timeline.group.{ellipse}.reversePath");
    assert!(h.state().auto.find(&format!("timeline.group.{fill}.reversePath")).is_none(), "only on path items");
    assert_eq!(dir(&h), effectcraft_engine::keyframe::Value::Enum(0));
    assert_eq!(h.state().auto.find(&button).map(|e| e.label.clone()).as_deref(), Some("Reverse Path Direction Off"));
    click(&mut h, &button);
    assert_eq!(dir(&h), effectcraft_engine::keyframe::Value::Enum(1));
    assert_eq!(h.state().auto.find(&button).map(|e| e.label.clone()).as_deref(), Some("Reverse Path Direction On"));
    // Path Direction stays hidden as a property row.
    let direction = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.find_group(ellipse).unwrap().get("direction").unwrap().uid;
    assert!(h.state().auto.find(&format!("timeline.prop.{direction}.row")).is_none(), "no Path Direction row");
    let size = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().props.find_group(ellipse).unwrap().get("size").unwrap().uid;
    assert!(h.state().auto.find(&format!("timeline.prop.{size}.row")).is_some(), "the ellipse is open (its Size row shows)");
    click(&mut h, &button);
    assert_eq!(dir(&h), effectcraft_engine::keyframe::Value::Enum(0));
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    assert_eq!(dir(&h), effectcraft_engine::keyframe::Value::Enum(1), "each click is one undo step");
}

/// #612: the real shape Gradient Fill property, driven through the complete app and its
/// timeline, commits one undoable document change. Optional local screenshots show the editor.
#[test]
fn gradient_fill_editor_applies_undoes_and_escape_discards() {
    use effectcraft_engine::keyframe::Value;
    let mut s = session();
    let layer = s.execute("layer.newShape", json!({"name": "Gradient rectangle"})).unwrap()["layer"].as_u64().unwrap();
    s.execute("layer.addShapeItem", json!({"layer": layer, "kind": "rect"})).unwrap();
    s.execute("layer.addShapeItem", json!({"layer": layer, "kind": "gradientFill"})).unwrap();
    let prop = s.active_comp().unwrap().layer(LayerId(layer)).unwrap().props.prop("contents/gfill/colors").unwrap();
    let uid = prop.uid;
    let original = prop.value.clone();
    let mut h = harness(s);
    h.state_mut().ui.timeline.search = "Colors".into();
    h.run_steps(3);
    let entry = format!("timeline.prop.{uid}.value");
    click(&mut h, &entry);
    click(&mut h, "dialog.gradient.addColor");
    let undo_before = h.state().session.history.undo.len();
    let value =
        |h: &Harness<'_, EffectcraftApp>| h.state().session.active_comp().unwrap().layer(LayerId(layer)).unwrap().props.find(uid).unwrap().value.clone();
    assert_eq!(value(&h), original, "editing the draft does not alter the project");
    if let Ok(dir) = std::env::var("EC_SNAPSHOT_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        h.render().expect("gradient editor screenshot").save(format!("{dir}/gradient_fill_editor.png")).unwrap();
    }
    click(&mut h, "dialog.gradient.ok");
    let Value::Gradient(edited) = value(&h) else { panic!("gradient fill lost its gradient value") };
    let Value::Gradient(before) = &original else { panic!("fixture must be a gradient") };
    assert_eq!(edited.colors.len(), before.colors.len() + 1);
    assert_eq!(h.state().session.history.undo.len(), undo_before + 1);
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    h.run_steps(3);
    assert_eq!(value(&h), original);
    click(&mut h, &entry);
    click(&mut h, "dialog.gradient.addOpacity");
    let project_before_escape = serde_json::to_value(&h.state().session.project).unwrap();
    let history_before_escape = h.state().session.history.undo.len();
    for pressed in [true, false] {
        h.input_mut().events.push(Event::Key { key: Key::Escape, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE });
        h.step();
    }
    h.run_steps(3);
    assert!(h.state().auto.find("dialog.gradient.ok").is_none(), "Escape closes the draft editor");
    assert_eq!(serde_json::to_value(&h.state().session.project).unwrap(), project_before_escape, "Escape never deletes a layer or changes the project");
    assert_eq!(h.state().session.history.undo.len(), history_before_escape);
    assert_eq!(value(&h), original);
}

/// Issue #633: a colour property in the Timeline (a shape's Fill Color) has an eyedropper after
/// its swatch, as in After Effects: click it, then click in the Composition panel to sample.
#[test]
fn timeline_colour_eyedropper_samples_the_comp() {
    let mut s = session();
    s.execute("layer.newSolid", json!({"name": "Back", "color": "#20c040"})).unwrap();
    let l = s.execute("layer.newShape", json!({"kind": "rect", "fill": [1.0, 0.0, 0.0], "size": [100.0, 60.0]})).unwrap()["layer"].as_u64().unwrap();
    let layer = s.active_comp().unwrap().layer(LayerId(l)).unwrap().clone();
    let mut fill = None;
    layer.props.walk("", &mut |_, p| {
        if fill.is_none() && matches!(p.value, effectcraft_engine::keyframe::Value::Color(_)) {
            fill = Some(p.uid);
        }
    });
    let fill = fill.expect("a fill colour");
    fn groups(g: &effectcraft_engine::project::PropGroup, out: &mut Vec<u64>) {
        for c in g.groups() {
            out.push(c.uid);
            groups(c, out);
        }
    }
    let mut open = vec![];
    groups(&layer.props, &mut open);
    let mut h = harness(s);
    h.state_mut().ui.timeline.open_groups.extend(open);
    open_layer(&mut h, l);
    h.run_steps(2);
    // Keep the fork's settling frames while using the shared upstream wait helper.
    settle(&mut h);
    h.run_steps(2);
    click(&mut h, &format!("timeline.prop.{fill}.eyedropper"));
    assert_eq!(h.state().ui.fx_pick.as_ref().map(|p| (p.prop, p.kind.as_str())), Some((fill, "color")), "the eyedropper waits for a viewer click");
    let pos = effectcraft_ui_egui::panels::viewer::comp_to_screen(&h.ctx, [6.0, 6.0]).unwrap();
    click_at(&mut h, pos);
    assert!(h.state().ui.fx_pick.is_none(), "the pick is used up");
    let layer = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().clone();
    let mut got = None;
    layer.props.walk("", &mut |_, p| {
        if p.uid == fill {
            got = Some(p.value.clone());
        }
    });
    let Some(effectcraft_engine::keyframe::Value::Color(c)) = got else { panic!("{got:?}") };
    let want = [0x20 as f64 / 255.0, 0xc0 as f64 / 255.0, 0x40 as f64 / 255.0];
    for k in 0..3 {
        assert!((c[k] - want[k]).abs() < 0.02, "{c:?} vs {want:?}");
    }
}

/// Ctrl+V with SVG markup on the system clipboard pastes it as a shape layer (After Effects
/// 26.3), even with layers on the app's clipboard: copying in the app replaces the system
/// clipboard, so the SVG was copied later (#738).
#[test]
fn ctrl_v_pastes_svg_markup_as_a_shape_layer() {
    let (mut h, a, b, _) = two_shape_layers();
    click(&mut h, &format!("timeline.layer.{b}.row"));
    h.input_mut().events.push(Event::Copy);
    h.step();
    assert_eq!(h.state().session.state.clipboard.len(), 1, "a layer on the app's clipboard");
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><circle cx="10" cy="10" r="8" fill="#00ff00"/><rect x="20" width="20" height="20"/></svg>"##;
    h.input_mut().events.push(Event::Paste(svg.into()));
    h.run_steps(3);
    let c = h.state().session.active_comp().unwrap().clone();
    assert_eq!(c.layers.len(), 3, "{:?}", h.state().ui.status);
    let top = &c.layers[0];
    assert!(top.id != LayerId(a) && top.id != LayerId(b));
    assert_eq!(top.name, "Shape Layer 3");
    assert_eq!(top.props.sub("contents").unwrap().groups().count(), 2);
    assert_eq!(h.state().session.state.selected_layers, vec![top.id]);
    // The app's own clipboard note pastes the copied layer as before.
    h.input_mut().events.push(Event::Paste("EffectCraft: 1 layer".into()));
    h.run_steps(3);
    let c = h.state().session.active_comp().unwrap();
    assert_eq!(c.layers.len(), 4);
    assert_eq!(c.layers[0].name, "Oval 2");
}

#[test]
fn shape_item_eye_toggles_nested_fill_and_undoes() {
    let mut s = session();
    let lid = s.execute("layer.newShape", json!({"kind":"rect","strokeWidth":2})).unwrap()["layer"].as_u64().unwrap();
    let layer = s.active_comp().unwrap().layer(LayerId(lid)).unwrap();
    let contents = layer.props.sub("contents").unwrap().uid;
    let group = layer.props.group("contents/group").unwrap().uid;
    let nested = layer.props.group("contents/group/contents").unwrap().uid;
    let fill = layer.props.group("contents/group/contents/fill").unwrap().uid;
    let stroke = layer.props.group("contents/group/contents/stroke").unwrap().uid;
    let dashes = layer.props.group("contents/group/contents/stroke/dashes").unwrap().uid;
    let mut h = harness(s);
    h.state_mut().ui.timeline.open_layers.insert(lid);
    h.state_mut().ui.timeline.open_groups.extend([contents, group, nested, stroke]);
    h.run_steps(3);
    assert!(h.state().auto.find(&format!("timeline.group.{stroke}.eye")).is_some());
    assert!(h.state().auto.find(&format!("timeline.group.{dashes}.eye")).is_none());
    // The open Stroke's many rows push Fill out of the small panel: close it to reach Fill.
    h.state_mut().ui.timeline.open_groups.remove(&stroke);
    h.run_steps(3);
    click(&mut h, &format!("timeline.group.{fill}.eye"));
    assert!(!h.state().session.active_comp().unwrap().layer(LayerId(lid)).unwrap().props.find_group(fill).unwrap().enabled);
    h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    assert!(h.state().session.active_comp().unwrap().layer(LayerId(lid)).unwrap().props.find_group(fill).unwrap().enabled);
    h.state_mut().ui.timeline.search = "Fill".into();
    h.run_steps(3);
    assert!(h.state().auto.find(&format!("timeline.group.{fill}.eye")).is_some(), "filtered rows keep their eye");
}

fn figcraft_clipboard() -> String {
    json!({"format":"figcraft.clipboard","version":1,"warnings":[],"nodes":[
        {"id":"frame","type":"FRAME","name":"FigCraft card","size":{"x":200,"y":100},
         "absoluteTransform":[[1,0,0],[0,1,0]],"children":[
          {"id":"rect","type":"RECTANGLE","name":"FigCraft rectangle","size":{"x":40,"y":20},
           "absoluteTransform":[[1,0,10],[0,1,15]],"fills":[{"type":"SOLID","color":{"r":1,"g":0,"b":0,"a":1}}]}]}
    ]})
    .to_string()
}

fn figcraft_svg_clipboard() -> String {
    let encoded = effectcraft_engine::track::roto::rle::base64_encode(figcraft_clipboard().as_bytes());
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"10\" height=\"10\"/><metadata id=\"figcraft-clipboard-v1\">{encoded}</metadata></svg>")
}

#[test]
fn figcraft_paste_works_in_an_empty_project_from_keyboard_and_menu() {
    for clipboard in [figcraft_clipboard(), figcraft_svg_clipboard()] {
        let mut h = harness(Session::default());
        h.state_mut().ui.focused = effectcraft_ui_egui::dock::PanelKind::Project;
        h.input_mut().events.push(Event::Paste(clipboard.clone()));
        h.run_steps(3);
        let comp = h.state().session.active_comp().unwrap();
        assert_eq!(comp.layers.len(), 1, "{:?}", h.state().ui.status);
        assert_eq!(comp.layers[0].name, "FigCraft rectangle");
        assert_eq!(h.state().session.history.undo.len(), 1);

        let mut app = EffectcraftApp::new(Session::default());
        app.ui.focused = effectcraft_ui_egui::dock::PanelKind::Project;
        app.hooks.clipboard_text = Some(Box::new(move || Some(clipboard.clone())));
        let entries = effectcraft_ui_egui::menus::menu_items(&app);
        assert!(entries.iter().any(|entry| entry.id == "edit.paste" && entry.enabled));
        effectcraft_ui_egui::menus::invoke(&mut app, &egui::Context::default(), "edit.paste", json!({})).unwrap();
        assert_eq!(app.session.active_comp().unwrap().layers.len(), 1);
    }
}

#[test]
fn damaged_figcraft_paste_does_not_paste_the_old_internal_clipboard() {
    let (mut h, _, b, _) = two_shape_layers();
    click(&mut h, &format!("timeline.layer.{b}.row"));
    h.input_mut().events.push(Event::Copy);
    h.step();
    let before = h.state().session.project.clone();
    for clipboard in [
        r#"{"format":"figcraft.clipboard","version":1,"nodes":broken"#,
        r#"<svg><rect width="10" height="10"/><metadata id="figcraft-clipboard-v1">%%%broken</metadata></svg>"#,
    ] {
        h.input_mut().events.push(Event::Paste(clipboard.into()));
        h.run_steps(3);
        assert_eq!(*h.state().session.project, *before);
        assert!(h.state().ui.status.contains("invalid FigCraft clipboard"), "{}", h.state().ui.status);
    }
}

fn effect_names(h: &Harness<'_, EffectcraftApp>, l: u64) -> Vec<String> {
    h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().effects().map(|f| f.groups().map(|g| g.name.clone()).collect()).unwrap_or_default()
}

/// Elias: Drop Shadow selected on a shape layer (its Contents open), ⌘C, another shape layer
/// selected, ⌘V: nothing was pasted. Keyframes selected before (clicking an animated
/// property's name selects its keys) stayed selected when the effect was clicked, so ⌘C copied
/// them instead of the effect. Clicking the effect in the Timeline or in Effect Controls copies
/// the effect, and ⌘V adds it to the selected layer with its values.
#[test]
fn cmd_c_and_cmd_v_copy_a_selected_effect_onto_another_shape_layer() {
    let mut s = session();
    let a = s.execute("layer.newShape", json!({"kind": "rect", "name": "A • player frame"})).unwrap()["layer"].as_u64().unwrap();
    let b = s.execute("layer.newShape", json!({"kind": "ellipse", "name": "A • player progress"})).unwrap()["layer"].as_u64().unwrap();
    let shadow = s.execute("effect.apply", json!({"layers": [a], "effect": "Drop Shadow"})).unwrap()["effects"][0].as_u64().unwrap();
    s.execute("prop.set", json!({"layer": a, "path": "effects/#1/distance", "value": 20.0})).unwrap();
    s.execute("prop.toggleAnimation", json!({"layer": a, "path": "transform/opacity"})).unwrap();
    s.execute("time.set", json!({"time": 1.0})).unwrap();
    s.execute("prop.set", json!({"layer": a, "path": "transform/opacity", "value": 40})).unwrap();
    s.execute("time.set", json!({"time": 0.0})).unwrap();
    let l = s.active_comp().unwrap().layer(LayerId(a)).unwrap().clone();
    let (contents, effects) = (l.props.sub("contents").unwrap().uid, l.props.sub("effects").unwrap().uid);
    let mut h = harness(s);
    h.state_mut().show_panel(effectcraft_ui_egui::dock::PanelKind::EffectControls);
    h.state_mut().ui.timeline.open_groups.extend([contents, effects]);
    open_layer(&mut h, a);
    let distance = |h: &Harness<'_, EffectcraftApp>, l: u64, n: usize| {
        let layer = h.state().session.active_comp().unwrap().layer(LayerId(l)).unwrap().clone();
        layer.effects().unwrap().groups().nth(n).unwrap().get("distance").unwrap().value.clone()
    };
    for (n, at) in [format!("timeline.group.{shadow}.name"), format!("effectControls.effect.{shadow}")].into_iter().enumerate() {
        // Opacity's name clicked before: its two keys are selected.
        h.state_mut().session.execute("layer.select", json!({"layers": [a]})).unwrap();
        h.state_mut().session.execute("prop.select", json!({"layer": a, "path": "transform/opacity"})).unwrap();
        assert_eq!(h.state().session.state.selected_keys.len(), 2);
        h.run_steps(3);
        click(&mut h, &at);
        assert_eq!(h.state().session.state.selected_props, vec![(LayerId(a), shadow)], "{at}");
        h.input_mut().events.push(Event::Copy);
        h.step();
        assert_eq!(h.state().session.state.effect_clipboard.len(), 1, "{at}: {}", h.state().ui.status);
        click(&mut h, &format!("timeline.layer.{b}.row"));
        assert_eq!(h.state().session.state.selected_layers, vec![LayerId(b)]);
        h.input_mut().events.push(Event::Paste("EffectCraft: 1 effect".into()));
        h.run_steps(3);
        assert_eq!(effect_names(&h, b).len(), n + 1, "{at}: {}", h.state().ui.status);
        assert_eq!(effect_names(&h, b)[n], if n == 0 { "Drop Shadow" } else { "Drop Shadow 2" });
        assert_eq!(distance(&h, b, n), effectcraft_engine::keyframe::Value::Scalar(20.0));
        assert_eq!(h.state().session.active_comp().unwrap().layers.len(), 2, "no layer pasted");
    }
    assert_eq!(effect_names(&h, a), ["Drop Shadow"]);
}
