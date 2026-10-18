use effectcraft_keyframe::{Keyframe, ShapePath, Value};
use effectcraft_time::{FrameRate, Tick};

use crate::build::{self, Ids};
use crate::{Comp, ItemKind, LayerSource, MaskMode, Project, Solid};

#[test]
fn comp_contains_terminates_for_a_cycle_that_does_not_contain_the_target() {
    let p = Project::from_json(include_str!("../../../tests/repros/freeze-cycle.ecproj")).unwrap();
    assert!(!p.comp_contains(crate::ItemId(2), crate::ItemId(1)));
    assert!(p.comp_contains(crate::ItemId(2), crate::ItemId(3)));
    assert!(p.comp_contains(crate::ItemId(3), crate::ItemId(2)));
}

fn project_with_layer() -> (Project, crate::ItemId) {
    let mut p = Project::default();
    let comp = Comp::new(1920, 1080, FrameRate::FPS_29_97, Tick::from_seconds_f64(10.0));
    let cid = p.add_item("Main", effectcraft_color::Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let sid = p.add_item(
        "Red Solid",
        effectcraft_color::Label::Red,
        None,
        ItemKind::Solid(Solid { color: [1.0, 0.0, 0.0], width: 1920, height: 1080, pixel_aspect: 1.0 }),
    );
    let layer = build::layer(&mut p, &comp, "Red Solid 1", LayerSource::Solid { item: sid }, (1920, 1080), None);
    p.comp_mut(cid).unwrap().layers.push(layer);
    (p, cid)
}

#[test]
fn paths_resolve() {
    let (p, cid) = project_with_layer();
    let l = &p.comp(cid).unwrap().layers[0];
    assert_eq!(l.props.prop("transform/position").unwrap().value, Value::Vec3([960.0, 540.0, 0.0]));
    assert_eq!(l.props.prop("Transform/Opacity").unwrap().value, Value::Scalar(100.0));
    assert_eq!(l.props.prop("transform/#7").unwrap().match_id, "rotation");
    let uid = l.props.prop("transform/scale").unwrap().uid;
    let path = l.props.path_of(uid).unwrap();
    assert_eq!(l.props.prop(&path).unwrap().match_id, "scale");
    assert_eq!(l.props.name_path_of(uid).unwrap(), "Transform/Scale");
}

#[test]
fn masks_and_occurrence_paths() {
    let (mut p, cid) = project_with_layer();
    let mut next = p.next_id;
    let m1 = build::mask(&mut Ids(&mut next), "Mask 1", ShapePath::rect([0.0, 0.0], 10.0, 10.0), MaskMode::Add, [255, 0, 0]);
    let m2 = build::mask(&mut Ids(&mut next), "Mask 2", ShapePath::ellipse([0.0, 0.0], 10.0, 10.0), MaskMode::Subtract, [0, 255, 0]);
    p.next_id = next;
    let l = &mut p.comp_mut(cid).unwrap().layers[0];
    let masks = l.props.sub_mut("masks").unwrap();
    masks.children.push(m1.into());
    masks.children.push(m2.into());
    assert_eq!(l.props.group("masks/mask#2").unwrap().name, "Mask 2");
    assert_eq!(l.props.group("masks/Mask 2").unwrap().name, "Mask 2");
    assert!(l.props.prop("masks/mask#2/feather").is_some());
}

#[test]
fn animate_and_roundtrip_json() {
    let (mut p, cid) = project_with_layer();
    {
        let l = &mut p.comp_mut(cid).unwrap().layers[0];
        let op = l.props.prop_mut("transform/opacity").unwrap();
        op.set_animated(true, Tick::ZERO);
        op.set_value_at(Tick::from_seconds_f64(1.0), Value::Scalar(0.0));
        assert_eq!(op.keys.len(), 2);
        assert_eq!(op.value_at(Tick::from_seconds_f64(0.5)), Value::Scalar(50.0));
    }
    let json = p.to_json();
    let q = Project::from_json(&json).unwrap();
    assert_eq!(p, q);
    let _ = Keyframe::new(Tick::ZERO, Value::Scalar(0.0));
}

#[test]
fn comp_resolution_is_saved_and_old_projects_load_with_auto() {
    use crate::Resolution;
    let (mut p, cid) = project_with_layer();
    // Auto (the default) isn't written, so older versions read the file as before.
    assert!(!p.to_file_json().unwrap().contains("\"resolution\""));
    p.comp_mut(cid).unwrap().resolution = Resolution::Custom(5);
    let q = Project::from_json(&p.to_file_json().unwrap()).unwrap();
    assert_eq!(q.comp(cid).unwrap().resolution, Resolution::Custom(5));
    p.comp_mut(cid).unwrap().resolution = Resolution::Half;
    let text = p.to_file_json().unwrap();
    assert_eq!(Project::from_json(&text).unwrap().comp(cid).unwrap().resolution, Resolution::Half);
    // A project saved before comps had a resolution opens with Auto.
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("resolution");
                m.values_mut().for_each(strip);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut old: serde_json::Value = serde_json::from_str(&text).unwrap();
    strip(&mut old);
    assert_eq!(Project::from_json(&old.to_string()).unwrap().comp(cid).unwrap().resolution, Resolution::Auto);
    // It's not part of the comp's pixels.
    let (a, b) = (p.comp(cid).unwrap(), q.comp(cid).unwrap());
    assert!(a != b && a.same_pixels(b));
    assert_eq!(format!("{:?}", a.pixel_form()), format!("{:?}", b.pixel_form()));
}

#[test]
fn resolution_factors_and_scales() {
    use crate::Resolution;
    assert_eq!(Resolution::from_factor(1), Resolution::Full);
    assert_eq!(Resolution::from_factor(4), Resolution::Quarter);
    assert_eq!(Resolution::from_factor(0), Resolution::Full);
    assert_eq!(Resolution::from_factor(7), Resolution::Custom(7));
    assert_eq!(Resolution::from_factor(1000), Resolution::Custom(40));
    assert_eq!(Resolution::Custom(5).factor(), Some(5));
    assert_eq!(Resolution::Auto.factor(), None);
    // A hostile factor from a file renders within the Custom range.
    assert_eq!(Resolution::Custom(0).scale(1.0, 1.0), 1.0);
    assert_eq!(Resolution::Custom(255).scale(1.0, 1.0), 1.0 / 40.0);
    assert_eq!(Resolution::Third.scale(0.1, 1.0), 1.0 / 3.0);
}

#[test]
fn unique_names_and_cycles() {
    let (p, cid) = project_with_layer();
    let c = p.comp(cid).unwrap();
    assert_eq!(c.unique_layer_name("Red Solid 1"), "Red Solid 2");
    assert_eq!(c.unique_layer_name("Shape Layer 1"), "Shape Layer 1");
    assert!(p.comp_contains(cid, cid));
}

#[test]
fn camera_and_light_layers_build() {
    let (mut p, cid) = project_with_layer();
    let comp = p.comp(cid).unwrap().clone();
    let cam = build::layer(&mut p, &comp, "Camera 1", LayerSource::Camera, (0, 0), None);
    assert!(cam.props.prop("cameraOptions/zoom").is_some());
    assert!(cam.props.prop("transform/poi").is_some());
    let light = build::layer(&mut p, &comp, "Light 1", LayerSource::Light { kind: crate::LightKind::Spot }, (0, 0), None);
    assert!(light.props.prop("lightOptions/coneAngle").is_some());
}

#[test]
fn same_content_sees_allocated_ids_and_comp_edits() {
    let (p, cid) = project_with_layer();
    let mut q = p.clone();
    assert!(q.same_content(&p));
    q.alloc();
    assert!(!q.same_content(&p), "an allocated id stays taken");
    let mut q = p.clone();
    // A comp copied on write but left equal is the same; a changed one is not.
    let _ = q.comp_mut(cid).unwrap();
    assert!(q.same_content(&p));
    q.comp_mut(cid).unwrap().layers[0].name = "Renamed".into();
    assert!(!q.same_content(&p));
    let mut r = p.clone();
    r.item_mut(cid).unwrap().name = "Other".into();
    assert!(!r.same_content(&p));
    let mut r = p.clone();
    r.settings.bit_depth = crate::BitDepth::Bpc16;
    assert!(!r.same_content(&p));
}

#[test]
fn project_files_name_their_version_and_always_read_back() {
    let (mut p, cid) = project_with_layer();
    let text = p.to_file_json().unwrap();
    assert_eq!(crate::saved_by(&text).as_deref(), Some(crate::APP_VERSION));
    assert!(text.starts_with("{\"savedBy\""), "the version comes first");
    assert!(!text.contains('\n'), "compact");
    assert_eq!(Project::from_json(&text).unwrap(), p);
    assert_eq!(crate::saved_by(&p.to_json()), None, "plain JSON (worker hand-offs) has none");
    // JSON can't hold NaN: saving refuses instead of writing a file that can't be opened.
    p.comp_mut(cid).unwrap().layers[0].props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(f64::NAN);
    let e = p.to_file_json().unwrap_err().to_string();
    assert!(e.contains("can't be saved"), "{e}");
    assert!(e.contains("isn't finite") && e.contains("/props/") && e.ends_with("/value/v)"), "names where: {e}");
    let k = Keyframe::new(Tick(0), Value::Scalar(1.0));
    p.comp_mut(cid).unwrap().layers[0].props.prop_mut("transform/opacity").unwrap().keys =
        vec![Keyframe { spatial_in: [0.0, f64::INFINITY, 0.0], ..k.clone() }];
    p.comp_mut(cid).unwrap().layers[0].props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(1.0);
    assert!(p.to_file_json().unwrap_err().to_string().contains("spatial_in/1"));
    // What the save check passes reads back, and what it refuses doesn't.
    assert!(crate::finite::check(&Some(f64::NAN)).is_ok() && serde_json::from_str::<Option<f64>>("null").is_ok(), "Some(NaN) reads back as None");
    assert!(crate::finite::check(&Some([f64::NAN])).is_err() && crate::finite::check(&vec![1.0f32, f32::NEG_INFINITY]).is_err());
    assert!(crate::finite::check(&std::collections::BTreeMap::from([(1u64, Some(f64::NAN))])).is_ok());
    assert!(crate::is_newer_version("0.2.0", "0.1.9") && crate::is_newer_version("1.0.0-rc.1", "0.9.9"));
    assert!(!crate::is_newer_version("0.1.1", "0.1.1") && !crate::is_newer_version("0.1.0", "0.1.1"));
}

/// Fields at their default value are left out of a project file and read back as it; files that
/// write them all (older versions, pretty-printed) still load the same.
#[test]
fn default_fields_are_left_out_and_read_back() {
    use crate::props::ParamUi;
    use effectcraft_keyframe::{Ease, Interp};
    let (mut p, cid) = project_with_layer();
    let layer = &mut p.comp_mut(cid).unwrap().layers[0];
    let plain = Keyframe::new(Tick(0), Value::Vec3([1.0, 2.0, 3.0]));
    let full = Keyframe {
        in_interp: Interp::Bezier,
        out_interp: Interp::Hold,
        in_ease: vec![Ease::EASY].into(),
        out_ease: vec![Ease { speed: 3.0, influence: 0.5 }].into(),
        continuous: true,
        auto_bezier: true,
        spatial_in: [1.0, -2.0, 0.0],
        spatial_out: [-0.0, 0.0, 0.0],
        spatial_auto: false,
        spatial_continuous: true,
        roving: true,
        ae_spatial: true,
        label: 3,
        ..Keyframe::new(Tick(1000), Value::Vec3([4.0, 5.0, 6.0]))
    };
    let pos = layer.props.prop_mut("transform/position").unwrap();
    pos.keys = vec![plain.clone(), full];
    let opacity = layer.props.prop_mut("transform/opacity").unwrap();
    opacity.ui = ParamUi::Slider { min: 0.0, max: 100.0, slider_min: 0.0, slider_max: 100.0, decimals: 1 };
    opacity.spatial = true;
    opacity.hold_only = true;
    opacity.three_d_only = true;
    opacity.two_d_only = true;
    opacity.shown_dims = 2;
    opacity.static_only = true;
    let rotation = layer.props.prop_mut("transform/rotation").unwrap();
    rotation.ui = ParamUi::Number;
    rotation.hold_only = false;

    let text = p.to_file_json().unwrap();
    let back = Project::from_json(&text).unwrap();
    assert_eq!(back, p);
    let keys = &back.comp(cid).unwrap().layers[0].props.prop("transform/position").unwrap().keys;
    assert_eq!(keys[1].spatial_out[0].to_bits(), (-0.0f64).to_bits(), "-0 keeps its sign");
    let plain_json = serde_json::to_value(&plain).unwrap();
    assert_eq!(plain_json, serde_json::json!({"time": 0, "value": {"t": "Vec3", "v": [1.0, 2.0, 3.0]}}), "a plain key writes only time and value");
    let rot = serde_json::to_value(back.comp(cid).unwrap().layers[0].props.prop("transform/rotation").unwrap()).unwrap();
    for k in ["ui", "spatial", "hold_only", "three_d_only", "two_d_only", "shown_dims", "static_only", "keys", "expr"] {
        assert!(rot.get(k).is_none(), "{k} left out: {rot}");
    }

    // An old-style file: every field written, pretty-printed.
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    fn fill(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(o) => {
                if o.contains_key("match") && o.contains_key("uid") {
                    for (k, d) in [
                        ("ui", serde_json::json!({"kind": "Number"})),
                        ("spatial", false.into()),
                        ("hold_only", false.into()),
                        ("three_d_only", false.into()),
                        ("two_d_only", false.into()),
                        ("shown_dims", 0.into()),
                        ("static_only", false.into()),
                    ] {
                        o.entry(k).or_insert(d);
                    }
                }
                if o.contains_key("time") && o.contains_key("value") && !o.contains_key("uid") {
                    for (k, d) in [
                        ("in_interp", "Linear".into()),
                        ("out_interp", "Linear".into()),
                        ("in_ease", serde_json::json!([])),
                        ("out_ease", serde_json::json!([])),
                        ("continuous", false.into()),
                        ("auto_bezier", false.into()),
                        ("spatial_in", serde_json::json!([0.0, 0.0, 0.0])),
                        ("spatial_out", serde_json::json!([0.0, 0.0, 0.0])),
                        ("spatial_auto", true.into()),
                        ("spatial_continuous", false.into()),
                        ("roving", false.into()),
                    ] {
                        o.entry(k).or_insert(d);
                    }
                }
                o.values_mut().for_each(fill);
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(fill),
            _ => {}
        }
    }
    fill(&mut v);
    let old = serde_json::to_string_pretty(&v).unwrap();
    assert!(old.len() > text.len() && old.contains("\"spatial_auto\": true") && old.contains("\"hold_only\": false"));
    assert_eq!(Project::from_json(&old).unwrap(), p);
}

/// `file_meta` reads `savedBy` and `openComps` from the ends of a file EffectCraft wrote, and
/// gives the same answer as a full pass for any other layout.
#[test]
fn file_meta_reads_the_ends_or_the_whole_file() {
    let (mut p, cid) = project_with_layer();
    // A string `"openComps"` that isn't the member.
    p.comp_mut(cid).unwrap().layers[0].name = "openComps".into();
    let open = crate::OpenComps { comps: vec![crate::OpenComp { comp: cid, time: Tick(7) }], active: Some(cid) };
    let with = p.to_file_json_with(Some(&open)).unwrap();
    let without = p.to_file_json().unwrap();
    let slow = |text: &str| {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        crate::FileMeta { saved_by: v["savedBy"].as_str().map(String::from), open_comps: serde_json::from_value(v["openComps"].clone()).ok() }
    };
    assert!(crate::file_meta_at_ends(&with).is_some(), "EffectCraft's own file");
    assert!(crate::file_meta_at_ends(&without).is_none(), "the layer name isn't the member: a full pass");
    let mut q = p.clone();
    q.comp_mut(cid).unwrap().layers[0].name = "x\"openComps".into();
    assert!(crate::file_meta_at_ends(&q.to_file_json().unwrap()).is_none());
    assert_eq!(crate::file_meta(&q.to_file_json().unwrap()).open_comps, None);
    q.comp_mut(cid).unwrap().layers[0].name = "Plain".into();
    assert_eq!(crate::file_meta_at_ends(&q.to_file_json().unwrap()), Some(crate::FileMeta { saved_by: Some(crate::APP_VERSION.into()), open_comps: None }));
    assert_eq!(crate::file_meta(&with), crate::FileMeta { saved_by: Some(crate::APP_VERSION.into()), open_comps: Some(open.clone()) });
    assert_eq!(crate::file_meta(&without), crate::FileMeta { saved_by: Some(crate::APP_VERSION.into()), open_comps: None });
    // Other layouts: pretty, keys sorted (`openComps` not last, `savedBy` not first), no version.
    let v: serde_json::Value = serde_json::from_str(&with).unwrap();
    let sorted = serde_json::to_string(&v).unwrap();
    let pretty = serde_json::to_string_pretty(&v).unwrap();
    let mut bare = v.clone();
    bare.as_object_mut().unwrap().remove("savedBy");
    let bare = serde_json::to_string(&bare).unwrap();
    for text in [&with, &without, &sorted, &pretty, &bare, &p.to_json()] {
        assert_eq!(crate::file_meta(text), slow(text), "{}", &text[..text.len().min(80)]);
    }
    assert_eq!(crate::file_meta("{\"savedBy\":\"1.0.0\",\"openComps\":5}").open_comps, None, "a malformed openComps is ignored");
    assert_eq!(crate::file_meta("{\"savedBy\":\"1.0.0\",\"openComps\":5}").saved_by.as_deref(), Some("1.0.0"));
}

#[test]
fn project_files_remember_the_open_comps() {
    let (p, cid) = project_with_layer();
    let view = crate::SavedView { active_comp: Some(cid), open_comps: vec![cid] };
    let text = p.to_file_json_with_view(&view).unwrap();
    assert_eq!(crate::saved_view(&text), view);
    assert_eq!(Project::from_json(&text).unwrap(), p, "the view is not part of the project");
    // Files from before #748 have no view; a garbled one is ignored, not an error.
    assert_eq!(crate::saved_view(&p.to_file_json().unwrap()), crate::SavedView::default());
    // Upstream's layout (`activeComp` and a list of ids) reads too, as a view and as `OpenComps`.
    let mut v: serde_json::Value = serde_json::from_str(&p.to_file_json().unwrap()).unwrap();
    v["activeComp"] = cid.0.into();
    v["openComps"] = serde_json::json!([cid.0]);
    let upstream = serde_json::to_string_pretty(&v).unwrap();
    assert_eq!(crate::saved_view(&upstream), view);
    assert_eq!(crate::file_meta(&upstream).open_comps, view.to_open_comps());
    v["activeComp"] = "Main".into();
    let odd = serde_json::to_string(&v).unwrap();
    assert_eq!(crate::saved_view(&odd), crate::SavedView { active_comp: None, open_comps: vec![cid] });
}

/// The Project panel's "used N times": layers in every comp that use the item directly, a precomp
/// counted by the comps it is nested in, and an unused item used by nothing.
#[test]
fn item_users_count_layers_in_every_comp() {
    let (mut p, main) = project_with_layer();
    let sid = p.find_by_name("Red Solid").unwrap().id;
    let comp = p.comp(main).unwrap().clone();
    let pre = p.add_item("A Precomp", effectcraft_color::Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let unused = p.add_item("Unused", effectcraft_color::Label::Red, None, ItemKind::Solid(Solid { color: [0.0; 3], width: 8, height: 8, pixel_aspect: 1.0 }));
    // Two more layers of the solid in the precomp (it starts with one, cloned from Main), and the
    // precomp nested in Main.
    for _ in 0..2 {
        let l = build::layer(&mut p, &comp, "Red Solid", LayerSource::Solid { item: sid }, (1920, 1080), None);
        p.comp_mut(pre).unwrap().layers.push(l);
    }
    let nest = build::layer(&mut p, &comp, "A Precomp", LayerSource::Comp { item: pre }, (1920, 1080), None);
    p.comp_mut(main).unwrap().layers.push(nest);
    // Ordered by comp name: "A Precomp" before "Main".
    assert_eq!(p.item_users(sid), vec![(pre, 3), (main, 1)]);
    assert_eq!(p.item_users(pre), vec![(main, 1)]);
    assert!(p.item_users(unused).is_empty());
    assert!(p.item_users(main).is_empty());
}
