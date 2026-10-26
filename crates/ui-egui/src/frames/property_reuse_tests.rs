use super::*;
use crate::frames::{FrameImage, Frames, to_color_image};
use effectcraft_engine::{Session, time::Tick};
use effectcraft_keyframe::{Ease, Interp, Keyframe};
use serde_json::json;

const TRANSFORMS: [&str; 4] = ["transform/position", "transform/scale", "transform/anchor", "transform/rotation"];

fn scene(path: &str, width: u32, height: u32) -> (Arc<Project>, ItemId) {
    let mut s = Session::default();
    let id = ItemId(
        s.execute("comp.new", json!({"name":"Transform retention", "width":width,"height":height,"duration":4,"frameRate":30})).unwrap()["comp"]
            .as_u64()
            .unwrap(),
    );
    s.execute("layer.newSolid", json!({"comp":id.0,"width":width/3,"height":height/5,"color":"#e02060"})).unwrap();
    let prop = Arc::make_mut(&mut s.project).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut(path).unwrap();
    prop.keys = [0., 1., 2.].into_iter().map(|t| Keyframe::new(Tick::from_seconds_f64(t), prop.value.clone())).collect();
    (s.project, id)
}
fn edit(before: &Arc<Project>, id: ItemId, path: &str, index: usize) -> Arc<Project> {
    let mut after = before.clone();
    let p = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut(path).unwrap();
    let key = p.keys.get_mut(index).unwrap();
    key.value = match (&key.value, path) {
        (Value::Vec3(v), "transform/position") => Value::Vec3([v[0] + 12., v[1] - 7., v[2]]),
        (Value::Vec3(_), "transform/scale") => Value::Vec3([60., 135., 100.]),
        (Value::Vec3(v), "transform/anchor") => Value::Vec3([v[0] + 5., v[1] - 3., v[2]]),
        (_, "transform/rotation") => Value::Scalar(37.),
        _ => panic!("test property"),
    };
    after
}
fn frame_key(content: u64, revision: u64, id: ItemId, frame: i64, scale: f64) -> FrameKey {
    FrameKey { content, revision, comp: id.0, frame, scale: (scale * 1000.) as u32, view: 7, opts: 11 }
}
fn render(project: &Project, id: ItemId, frame: i64, scale: f64) -> egui::ColorImage {
    let time = project.comp(id).unwrap().frame_rate.tick_of(frame);
    to_color_image(&effectcraft_engine::render::render_frame(project, id, time, scale))
}

#[test]
fn transform_reuse_actual_pixels_cache_variants_accounting_and_undo() {
    for path in TRANSFORMS {
        let (before, id) = scene(path, 64, 64);
        let after = edit(&before, id, path, 1);
        let frames = Frames::default();
        let old = frames.content_of(&before, 1, id);
        for scale in [1., 0.5] {
            for frame in [0, 15, 30, 45, 60, 90] {
                let key = frame_key(old, 1, id, frame, scale);
                frames.cache.lock().unwrap().insert(key, FrameImage::Cpu(Arc::new(render(&before, id, frame, scale))));
            }
        }
        let protected = frame_key(old, 1, id, 90, 1.);
        frames.cache.lock().unwrap().protected.insert(protected, 3);
        let bytes = frames.cache.lock().unwrap().bytes;
        let content = frames.content_of(&after, 2, id);
        for scale in [1., 0.5] {
            for frame in [0, 15, 30, 45, 60, 90] {
                let expected = render(&after, id, frame, scale);
                let key = frame_key(content, 2, id, frame, scale);
                let retained = matches!(frame, 0 | 60 | 90);
                assert_eq!(frames.is_cached(&key), retained, "{path} {frame} {scale}");
                if retained {
                    let cache = frames.cache.lock().unwrap();
                    let FrameImage::Cpu(actual) = cache.map.get(&key).unwrap().image().unwrap() else { panic!("CPU fixture") };
                    assert_eq!(actual.pixels, expected.pixels);
                } else {
                    assert_ne!(render(&before, id, frame, scale).pixels, expected.pixels);
                }
            }
        }
        assert_eq!(frames.cache.lock().unwrap().bytes, bytes);
        assert_eq!(frames.cache.lock().unwrap().order.len(), 12);
        let moved = FrameKey { content, revision: 2, ..protected };
        assert_eq!(frames.cache.lock().unwrap().protected.get(&moved), Some(&3));
        assert!(!frames.is_cached(&FrameKey { view: 8, ..moved }));
        assert!(!frames.is_cached(&FrameKey { opts: 12, ..moved }));
        assert!(!frames.is_cached(&FrameKey { scale: 250, ..moved }));
        let undo = frames.content_of(&before, 3, id);
        assert!(frames.is_cached(&frame_key(undo, 3, id, 90, 1.)));
        let redo = frames.content_of(&after, 4, id);
        assert!(frames.is_cached(&frame_key(redo, 4, id, 90, 1.)));
        assert_eq!(frames.cache.lock().unwrap().bytes, bytes);
    }
}

#[test]
fn transform_reuse_interpolation_endpoint_and_stretch_follow_renderer() {
    for path in TRANSFORMS {
        for interpolation in [Interp::Linear, Interp::Hold, Interp::Bezier] {
            for stretch in [100., 200., -100.] {
                let (mut before, id) = scene(path, 64, 64);
                let layer = Arc::make_mut(&mut before).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
                layer.start_time = Tick::from_seconds_f64(if stretch < 0. { 3. } else { 0.5 });
                layer.stretch = stretch;
                let prop = layer.props.prop_mut(path).unwrap();
                for k in &mut prop.keys {
                    k.in_interp = interpolation;
                    k.out_interp = interpolation;
                    k.in_ease = vec![Ease::EASY].into();
                    k.out_ease = vec![Ease::EASY].into();
                }
                let after = edit(&before, id, path, 1);
                let proof = Proof::between(&before, &after, id).unwrap();
                for frame in [0, 15, 30, 45, 60, 75, 90, 105] {
                    if proof.unchanged(id, frame) {
                        assert_eq!(render(&before, id, frame, 1.).pixels, render(&after, id, frame, 1.).pixels, "{path} {interpolation:?} {stretch} {frame}");
                    }
                }
            }
        }
        let (before, id) = scene(path, 64, 64);
        let endpoint = edit(&before, id, path, 2);
        assert!(!Proof::between(&before, &endpoint, id).unwrap().unchanged(id, 90));
        let leading = edit(&before, id, path, 0);
        assert!(!Proof::between(&before, &leading, id).unwrap().unchanged(id, -30));
    }
}

#[test]
fn transform_reuse_spatial_tangent_edits_use_evaluated_position() {
    let (mut before, id) = scene("transform/position", 64, 64);
    let prop = Arc::make_mut(&mut before).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut("transform/position").unwrap();
    for key in &mut prop.keys {
        key.spatial_auto = false;
    }
    let mut after = before.clone();
    let prop = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut("transform/position").unwrap();
    prop.keys[0].spatial_out = [24., 8., 0.];
    prop.keys[1].spatial_in = [12., 4., 0.];
    let proof = Proof::between(&before, &after, id).unwrap();
    assert!(proof.unchanged(id, 0));
    assert!(proof.unchanged(id, 90));
    assert!(!proof.unchanged(id, 15));
    assert_ne!(render(&before, id, 15, 1.).pixels, render(&after, id, 15, 1.).pixels);
    assert_eq!(render(&before, id, 90, 1.).pixels, render(&after, id, 90, 1.).pixels);
}

#[test]
fn transform_reuse_rejects_neighbor_orientation_separation_and_compound_edits() {
    let (mut before, id) = scene("transform/position", 64, 64);
    Arc::make_mut(&mut before).comp_mut(id).unwrap().layers.get_mut(0).unwrap().auto_orient = AutoOrient::AlongPath;
    let after = edit(&before, id, "transform/position", 1);
    let old = before.comp(id).unwrap();
    let new = after.comp(id).unwrap();
    let a = EvalCtx::new(&before, id, old, Tick::ZERO);
    let b = EvalCtx::new(&after, id, new, Tick::ZERO);
    assert_eq!(a.position(&old.layers[0], old.layers[0].transform().unwrap()), b.position(&new.layers[0], new.layers[0].transform().unwrap()));
    assert_ne!(a.layer_to_comp(&old.layers[0]).0, b.layer_to_comp(&new.layers[0]).0, "neighbor samples change orientation at the unchanged endpoint");
    assert!(Proof::between(&before, &after, id).is_none());
    for path in TRANSFORMS {
        let (before, id) = scene(path, 64, 64);
        for mode in 0..5 {
            let mut old = before.clone();
            if mode == 0 {
                let tr = Arc::make_mut(&mut old).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.sub_mut("transform").unwrap();
                let mut p = tr.get("position").unwrap().clone();
                p.match_id = "positionX".into();
                p.value = Value::Scalar(32.);
                p.keys.clear();
                tr.children.push(Node::Prop(p));
            }
            let mut after = edit(&old, id, path, 1);
            let l = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
            match mode {
                1 => l.props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(75.),
                2 => l.props.prop_mut(path).unwrap().expr = Some(Default::default()),
                3 => {
                    l.props.prop_mut(path).unwrap().keys[1].value =
                        if path == "transform/rotation" { Value::Scalar(f64::NAN) } else { Value::Vec3([0., f64::INFINITY, 0.]) }
                }
                4 => l.props.prop_mut(path).unwrap().value = Value::Vec2([1., 2.]),
                _ => (),
            }
            assert!(Proof::between(&old, &after, id).is_none(), "{path} mode{mode}");
        }
    }
}

#[test]
#[ignore = "native serial CPU benchmark; coordinate exclusive timing"]
fn transform_reuse_real_1080_cache_benchmark() {
    use std::time::Instant;
    let sequence = [0, 6, 12, 18, 24, 30, 60, 66, 72, 78, 84, 90];
    for path in ["transform/position", "transform/rotation"] {
        let (before, id) = scene(path, 1920, 1080);
        for endpoint in [false, true] {
            let after = edit(&before, id, path, if endpoint { 2 } else { 1 });
            let requested: Vec<i64> = if endpoint { (60..72).collect() } else { sequence.to_vec() };
            for scale in [1., 0.5] {
                // Actual 1080/540 rendered CPU images, not placeholder cache payloads.
                let cached: Vec<_> = requested.iter().map(|f| Arc::new(render(&before, id, *f, scale))).collect();
                let reference: Vec<_> = requested.iter().map(|f| Arc::new(render(&after, id, *f, scale))).collect();
                for enabled in [false, true, true, false] {
                    let frames = Frames::default();
                    let old = frames.content_of(&before, 1, id);
                    for (frame, image) in requested.iter().zip(&cached) {
                        frames.cache.lock().unwrap().insert(frame_key(old, 1, id, *frame, scale), FrameImage::Cpu(image.clone()));
                    }
                    let bytes = frames.cache.lock().unwrap().bytes;
                    assert_eq!(cached.first().unwrap().size, if scale == 1. { [1920, 1080] } else { [960, 540] });
                    assert_eq!(bytes, requested.len() * cached.first().unwrap().pixels.len() * 4);
                    let start = Instant::now();
                    let content = if enabled { frames.content_of(&after, 2, id) } else { super::super::comp_content(&after, id) };
                    let proof_ms = start.elapsed().as_secs_f64() * 1000.;
                    let service = Instant::now();
                    let mut renders = 0;
                    let mut images = Vec::new();
                    for frame in &requested {
                        let key = frame_key(content, 2, id, *frame, scale);
                        let hit = frames.cache.lock().unwrap().map.get(&key).and_then(|s| s.image().cloned());
                        let image = if let Some(FrameImage::Cpu(image)) = hit {
                            image
                        } else {
                            renders += 1;
                            Arc::new(render(&after, id, *frame, scale))
                        };
                        images.push(image);
                    }
                    let service_ms = service.elapsed().as_secs_f64() * 1000.;
                    for (a, b) in images.iter().zip(&reference) {
                        assert_eq!(a.pixels, b.pixels);
                    }
                    assert_eq!(renders, if enabled && !endpoint { 5 } else { 12 });
                    eprintln!(
                        "RT5_TRANSFORM_REAL {}",
                        json!({"path":path,"endpoint":endpoint,"scale":scale,"enabled":enabled,"frames":requested.len(),"cacheBytes":bytes,"renders":renders,"proofMs":proof_ms,"serviceMs":service_ms,"totalMs":proof_ms+service_ms})
                    );
                }
            }
        }
    }
}

const MASK_PROPERTIES: [&str; 3] = ["opacity", "feather", "expansion"];

fn mask_scene(name: &str) -> (Arc<Project>, ItemId, String) {
    let mut s = Session::default();
    let id =
        ItemId(s.execute("comp.new", json!({"name":"Mask retention","width":64,"height":64,"duration":4,"frameRate":30})).unwrap()["comp"].as_u64().unwrap());
    let layer = s.execute("layer.newSolid", json!({"comp":id.0,"width":64,"height":64,"color":"#e02060"})).unwrap()["layer"].as_u64().unwrap();
    for _ in 0..2 {
        s.execute("mask.new", json!({"layer":layer,"vertices":[[16,16],[48,16],[48,48],[16,48]],"closed":true})).unwrap();
    }
    let path = format!("masks/#2/{name}");
    let l = Arc::make_mut(&mut s.project).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
    l.props.sub_mut("masks").unwrap().children.get_mut(0).unwrap().as_group_mut().unwrap().enabled = false;
    let p = l.props.prop_mut(&path).unwrap();
    p.keys = [0., 1., 2.].into_iter().map(|t| Keyframe::new(Tick::from_seconds_f64(t), p.value.clone())).collect();
    (s.project, id, path)
}

fn mask_edit(before: &Arc<Project>, id: ItemId, path: &str, key: usize) -> Arc<Project> {
    let mut after = before.clone();
    let p = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut(path).unwrap();
    p.keys.get_mut(key).unwrap().value = if path.ends_with("/feather") {
        Value::Vec2([12., 6.])
    } else if path.ends_with("/opacity") {
        Value::Scalar(40.)
    } else {
        Value::Scalar(6.)
    };
    after
}

#[test]
fn mask_reuse_actual_pixels_cache_variants_accounting_and_undo() {
    for name in MASK_PROPERTIES {
        let (before, id, path) = mask_scene(name);
        let after = mask_edit(&before, id, &path, 1);
        let frames = Frames::default();
        let old = frames.content_of_preview(&before, 1, 101, id);
        for scale in [1., 0.5] {
            for frame in [0, 15, 30, 45, 60, 90] {
                frames.cache.lock().unwrap().insert(frame_key(old, 1, id, frame, scale), FrameImage::Cpu(Arc::new(render(&before, id, frame, scale))));
            }
        }
        let protected = frame_key(old, 1, id, 90, 1.);
        frames.cache.lock().unwrap().protected.insert(protected, 3);
        let bytes = frames.cache.lock().unwrap().bytes;
        let content = frames.content_of_preview(&after, 2, 202, id);
        for scale in [1., 0.5] {
            for frame in [0, 15, 30, 45, 60, 90] {
                let expected = render(&after, id, frame, scale);
                let key = frame_key(content, 2, id, frame, scale);
                let retained = matches!(frame, 0 | 60 | 90);
                assert_eq!(frames.is_cached(&key), retained, "{path} {frame} {scale}");
                if retained {
                    let cache = frames.cache.lock().unwrap();
                    let FrameImage::Cpu(actual) = cache.map.get(&key).unwrap().image().unwrap() else { panic!("CPU fixture") };
                    assert_eq!(actual.pixels, expected.pixels, "{path} {frame} {scale}");
                } else {
                    assert_ne!(render(&before, id, frame, scale).pixels, expected.pixels, "{path} {frame} {scale}");
                }
            }
        }
        assert_eq!(frames.cache.lock().unwrap().bytes, bytes);
        assert_eq!(frames.cache.lock().unwrap().order.len(), 12);
        let moved = FrameKey { content, revision: 2, ..protected };
        assert_eq!(frames.cache.lock().unwrap().protected.get(&moved), Some(&3));
        assert!(!frames.is_cached(&FrameKey { view: 8, ..moved }));
        assert!(!frames.is_cached(&FrameKey { opts: 12, ..moved }));
        assert!(!frames.is_cached(&FrameKey { scale: 250, ..moved }));
        let undo = frames.content_of(&before, 3, id);
        assert!(frames.is_cached(&frame_key(undo, 3, id, 90, 1.)));
        let redo = frames.content_of(&after, 4, id);
        assert!(frames.is_cached(&frame_key(redo, 4, id, 90, 1.)));
        assert_eq!(frames.cache.lock().unwrap().bytes, bytes);
    }
}

#[test]
fn mask_reuse_interpolation_endpoints_and_stretch_follow_renderer() {
    for name in MASK_PROPERTIES {
        for interpolation in [Interp::Linear, Interp::Hold, Interp::Bezier] {
            for stretch in [100., 200., -100.] {
                let (mut before, id, path) = mask_scene(name);
                let layer = Arc::make_mut(&mut before).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
                layer.start_time = Tick::from_seconds_f64(if stretch < 0. { 3. } else { 0.5 });
                layer.stretch = stretch;
                for k in &mut layer.props.prop_mut(&path).unwrap().keys {
                    k.in_interp = interpolation;
                    k.out_interp = interpolation;
                    k.in_ease = vec![Ease::EASY].into();
                    k.out_ease = vec![Ease::EASY].into();
                }
                let after = mask_edit(&before, id, &path, 1);
                let proof = Proof::between(&before, &after, id).unwrap();
                let mut retained = 0;
                for frame in [0, 15, 30, 45, 60, 75, 90, 105] {
                    if proof.unchanged(id, frame) {
                        retained += 1;
                        assert_eq!(render(&before, id, frame, 1.).pixels, render(&after, id, frame, 1.).pixels, "{path} {interpolation:?} {stretch} {frame}");
                    }
                }
                assert!(retained > 0);
            }
        }
        let (before, id, path) = mask_scene(name);
        let after = mask_edit(&before, id, &path, 0);
        assert!(!Proof::between(&before, &after, id).unwrap().unchanged(id, -30));
        let after = mask_edit(&before, id, &path, 2);
        assert!(!Proof::between(&before, &after, id).unwrap().unchanged(id, 90));
    }
}

#[test]
fn mask_reuse_rejects_expression_interpretation_compound_and_invalid_values() {
    for name in MASK_PROPERTIES {
        for mode in 0..8 {
            let (before, id, path) = mask_scene(name);
            let mut after = mask_edit(&before, id, &path, 1);
            let layer = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
            match mode {
                0 => layer.props.prop_mut("transform/opacity").unwrap().expr = Some(Default::default()),
                1 => layer.props.prop_mut(&path).unwrap().expr = Some(Default::default()),
                2 => layer.props.prop_mut(&path).unwrap().name = "Changed metadata".into(),
                3 => layer.props.prop_mut("masks/#1/opacity").unwrap().value = Value::Scalar(50.),
                4 => {
                    layer.props.prop_mut(&path).unwrap().keys[1].value =
                        if name == "feather" { Value::Vec2([f64::NAN, 0.]) } else { Value::Scalar(f64::INFINITY) }
                }
                5 => layer.props.prop_mut(&path).unwrap().value = Value::Vec3([1., 2., 3.]),
                6 => layer.props.group_mut("masks/#2").unwrap().enabled = false,
                7 => {
                    let GroupKind::Mask { inverted, .. } = &mut layer.props.group_mut("masks/#2").unwrap().kind else { panic!("mask") };
                    *inverted = true;
                }
                _ => (),
            }
            assert!(Proof::between(&before, &after, id).is_none(), "{path} mode {mode}");
        }
    }
}

#[test]
fn mask_reuse_forced_blur_rejects_coverage_properties_but_preserves_opacity() {
    for name in MASK_PROPERTIES {
        let (mut before, id, path) = mask_scene(name);
        let comp = Arc::make_mut(&mut before).comp_mut(id).unwrap();
        // New comps start with Motion Blur off (G2); the masks' blur needs it on.
        comp.enable_motion_blur = true;
        comp.shutter_angle = 360.;
        comp.shutter_phase = 0.;
        let layer = comp.layers.get_mut(0).unwrap();
        for (index, key) in layer.props.prop_mut(&path).unwrap().keys.iter_mut().enumerate() {
            key.time = Tick::from_seconds_f64(index as f64 / 30.);
        }
        let mask = layer.props.group_mut("masks/#2").unwrap();
        let GroupKind::Mask { motion_blur, .. } = &mut mask.kind else { panic!("mask") };
        *motion_blur = MaskMotionBlur::On;
        let p = mask.get_mut("path").unwrap();
        p.keys = [0., 1., 2.].into_iter().map(|t| Keyframe::new(Tick::from_seconds_f64(t), p.value.clone())).collect();
        let after = mask_edit(&before, id, &path, 1);
        if name == "opacity" {
            let proof = Proof::between(&before, &after, id).unwrap();
            assert!(proof.unchanged(id, 0));
            assert!(!proof.unchanged(id, 1));
            assert_eq!(render(&before, id, 0, 1.).pixels, render(&after, id, 0, 1.).pixels);
        } else {
            let old_comp = before.comp(id).unwrap();
            let new_comp = after.comp(id).unwrap();
            let old_layer = &old_comp.layers[0];
            let new_layer = &new_comp.layers[0];
            let a = EvalCtx::new(&before, id, old_comp, Tick::ZERO);
            let b = EvalCtx::new(&after, id, new_comp, Tick::ZERO);
            assert_eq!(a.value(old_layer, old_layer.props.prop(&path).unwrap()), b.value(new_layer, new_layer.props.prop(&path).unwrap()));
            assert_ne!(render(&before, id, 0, 1.).pixels, render(&after, id, 0, 1.).pixels, "shutter samples differ despite equal main-time {path}");
            assert!(Proof::between(&before, &after, id).is_none());
        }
    }
}
