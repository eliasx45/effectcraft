use effectcraft_engine::Session;
use effectcraft_ui_egui::{EffectcraftApp, dock::PanelKind};
use egui::{Event, PointerButton, Rect, pos2, vec2};
use egui_kittest::Harness;
use serde_json::json;

fn rect(h: &Harness<'_, EffectcraftApp>, id: &str) -> Rect {
    let el = h.state().auto.elements.iter().chain(h.state().auto.previous.iter()).find(|e| e.id == id).unwrap_or_else(|| panic!("missing {id}"));
    Rect::from_min_size(pos2(el.rect[0], el.rect[1]), vec2(el.rect[2], el.rect[3]))
}

fn click(h: &mut Harness<'_, EffectcraftApp>, id: &str) {
    let pos = rect(h, id).center();
    h.event(Event::PointerMoved(pos));
    h.step();
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Default::default() });
    h.step();
    h.event(Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Default::default() });
    h.step();
    h.run_steps(2);
}

#[test]
fn smart_freeze_panel_controls_use_real_click_targets_and_preserve_originals() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Freeze UI","width":32,"height":18,"duration":0.2,"frameRate":10})).unwrap();
    s.execute("layer.newSolid", json!({"name":"Original","color":"#ff0000"})).unwrap();
    s.execute("freeze.freeze", json!({"wait":true,"quickDraft":false})).unwrap();
    let id = s.project.freeze.sections[0].id;
    let root = effectcraft_engine::freeze_storage::root(&s.project.freeze.namespace).unwrap();
    let original = s.project.items.clone();
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_eframe(|_| {
        let mut app = EffectcraftApp::new(s);
        app.raise_panel(PanelKind::SmartFreeze);
        app.ui.maximized = Some(PanelKind::SmartFreeze);
        app
    });
    h.run_steps(3);
    click(&mut h, &format!("smartFreeze.draft.{id}"));
    assert!(h.state().session.project.freeze.sections[0].quick_draft);
    click(&mut h, &format!("smartFreeze.draft.{id}"));
    assert!(!h.state().session.project.freeze.sections[0].quick_draft);
    click(&mut h, &format!("smartFreeze.auto.{id}"));
    assert!(h.state().session.project.freeze.sections[0].auto);
    h.run_steps(3);
    assert!(h.state().session.project.freeze.sections[0].auto, "drawing must not toggle Auto back off");
    click(&mut h, &format!("smartFreeze.edit.{id}"));
    assert!(!h.state().session.project.freeze.sections[0].enabled);
    click(&mut h, &format!("smartFreeze.refresh.{id}"));
    h.state_mut().session.freeze_runtime.pause_until.store(0, std::sync::atomic::Ordering::Relaxed);
    h.state_mut().session.wait_jobs();
    h.run_steps(3);
    assert!(h.state().session.project.freeze.sections[0].enabled, "status={} jobs={:?}", h.state().ui.status, h.state().session.job_log);
    assert_eq!(h.state().session.project.items, original);
    click(&mut h, "smartFreeze.originals");
    assert!(!h.state().session.project.freeze.sections[0].auto);
    assert!(!h.state().session.project.freeze.sections[0].enabled);
    drop(h);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_heavy_layer_gets_a_timeline_hint_whose_button_freezes_it_and_whose_cross_dismisses_it() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Heavy UI","width":32,"height":18,"duration":0.2,"frameRate":10})).unwrap();
    s.execute("layer.newSolid", json!({"name":"Stat Stack","color":"#ff0000"})).unwrap();
    let comp = s.active_comp_id().unwrap();
    let layer = s.active_comp().unwrap().layers[0].id;
    for _ in 0..12 {
        s.note_layer_costs(comp, [(comp, layer, 1800.0)]);
    }
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s));
    h.run_steps(3);
    assert!(h.state().auto.elements.iter().chain(h.state().auto.previous.iter()).any(|e| e.id == "timeline.freezeHint"), "the hint is drawn");
    assert!(h.state().session.project.freeze.sections.is_empty(), "a hint alone freezes nothing");
    click(&mut h, "timeline.freezeHint.dismiss");
    h.run_steps(2);
    assert!(!h.state().auto.elements.iter().any(|e| e.id == "timeline.freezeHint"), "dismissed");
    assert!(h.state().session.project.freeze.sections.is_empty());

    let mut s2 = Session::default();
    s2.execute("comp.new", json!({"name":"Heavy UI 2","width":32,"height":18,"duration":0.2,"frameRate":10})).unwrap();
    s2.execute("layer.newSolid", json!({"name":"Stat Stack","color":"#ff0000"})).unwrap();
    let comp = s2.active_comp_id().unwrap();
    let layer = s2.active_comp().unwrap().layers[0].id;
    for _ in 0..12 {
        s2.note_layer_costs(comp, [(comp, layer, 1800.0)]);
    }
    drop(h);
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(s2));
    h.run_steps(3);
    click(&mut h, "timeline.freezeHint.freeze");
    h.state_mut().session.freeze_runtime.pause_until.store(0, std::sync::atomic::Ordering::Relaxed);
    h.state_mut().session.wait_jobs();
    h.run_steps(3);
    let sections = &h.state().session.project.freeze.sections;
    assert_eq!(sections.len(), 1, "status={}", h.state().ui.status);
    assert!(sections[0].auto, "a frozen hint stays fresh");
    let root = effectcraft_engine::freeze_storage::root(&h.state().session.project.freeze.namespace).unwrap();
    h.run_steps(3);
    let result = h.state().auto.elements.iter().chain(h.state().auto.previous.iter()).find(|e| e.id == "timeline.freezeHint.result");
    assert_eq!(result.map(|e| e.label.as_str()), Some("Frozen ✓ Stat Stack"), "the click ends in a visible result");
    assert!(!h.state().auto.elements.iter().any(|e| e.id == "timeline.freezeHint"), "frozen layers are not suggested again");
    drop(h);
    std::fs::remove_dir_all(root).ok();
}

fn settle_viewer(h: &mut Harness<'_, EffectcraftApp>) {
    for _ in 0..600 {
        h.step();
        if h.state().frames.inflight() == 0 && h.state().frames.last_ms.lock().is_ok_and(|v| *v > 0.0) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(3);
}

#[test]
fn smart_freeze_viewer_really_uses_the_cache_then_edit_restores_live_pixels() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name":"Frozen viewer","width":32,"height":18,"duration":0.2,"frameRate":10})).unwrap();
    s.execute("layer.newSolid", json!({"color":"#ff0000"})).unwrap();
    let item = s.active_comp().unwrap().layers[0].source.item().unwrap();
    s.execute("freeze.freeze", json!({"wait":true,"quickDraft":false})).unwrap();
    let id = s.project.freeze.sections[0].id;
    let root = effectcraft_engine::freeze_storage::root(&s.project.freeze.namespace).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 900.0)).build_eframe(|_| EffectcraftApp::new(s));
    settle_viewer(&mut h);
    let frozen = h.state_mut().viewer_pixels().unwrap();
    h.state_mut()
        .session
        .edit("Change original color", None, |p, _| {
            if let effectcraft_engine::project::ItemKind::Solid(s) = &mut p.item_mut(item).unwrap().kind {
                s.color = [0.0, 1.0, 0.0];
            }
            Ok(())
        })
        .unwrap();
    settle_viewer(&mut h);
    assert_eq!(h.state_mut().viewer_pixels().unwrap().pixels, frozen.pixels, "manual preview must keep using the saved cache");
    h.state_mut().session.execute("freeze.edit", json!({"section":id})).unwrap();
    settle_viewer(&mut h);
    assert_ne!(h.state_mut().viewer_pixels().unwrap().pixels, frozen.pixels, "Edit must switch the viewer to the changed original");
    drop(h);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn smart_freeze_gpu_frames_match_live_2d_and_3d() {
    use effectcraft_engine::render::{Accelerator, Backend, RenderOpts, Renderer};
    use effectcraft_engine::time::Tick;
    use std::sync::Arc;
    let _lock = crate::gpu_lock();
    let Some(gpu) = effectcraft_gpu::Gpu::headless() else {
        eprintln!("Smart Freeze GPU comparison skipped: no adapter");
        return;
    };
    eprintln!("Smart Freeze GPU adapter: {}", gpu.name());
    let gpu = Arc::new(gpu);
    for three_d in [false, true] {
        let mut s = Session { accel: Some(gpu.clone()), ..Default::default() };
        s.execute("comp.new", json!({"name":"GPU freeze","width":320,"height":180,"duration":0.2,"frameRate":10})).unwrap();
        let layer = s.execute("layer.newSolid", json!({"color":"#6746e8","width":120,"height":70})).unwrap()["layer"].as_u64().unwrap();
        s.execute("effect.apply", json!({"effect":"Gaussian Blur"})).unwrap();
        s.execute("prop.set", json!({"layer":layer,"path":"effects/#1/blurriness","value":8})).unwrap();
        if three_d {
            s.execute("layer.setSwitch", json!({"layer":layer,"switch":"threeD","value":true})).unwrap();
            s.execute("prop.set", json!({"layer":layer,"path":"transform/rotationY","value":18})).unwrap();
            s.execute("layer.newCamera", json!({})).unwrap();
        }
        let comp = s.active_comp_id().unwrap();
        let opts = RenderOpts { backend: Backend::Gpu, guides: true, ..Default::default() };
        let mut live = Renderer::new(&s.project, s.footage.as_ref(), opts);
        live.accel = Some(gpu.as_ref());
        let before = effectcraft_engine::render::passes::block_on(effectcraft_engine::render::passes::comp_frame(&live, comp, Tick::ZERO));
        s.execute("freeze.freeze", json!({"mode":"comp","wait":true,"quickDraft":false})).unwrap();
        let preview = effectcraft_engine::freeze::Preview::for_comp(&s.project, comp);
        let mut frozen = Renderer::new(&s.project, s.footage.as_ref(), opts);
        frozen.freeze = Some(&preview);
        let after = frozen.comp_frame(comp, Tick::ZERO);
        let mut worst = 0.0f32;
        let mut over = 0usize;
        assert_eq!((before.width, before.height), (after.width, after.height));
        for (a, b) in before.data.iter().zip(&after.data) {
            let delta = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
            worst = worst.max(delta);
            over += usize::from(delta > 1.01 / 255.0);
        }
        eprintln!("Smart Freeze 3d={three_d}: max={worst}, pixels over 1/255={over}/{}", before.data.len());
        assert!(worst <= 4.01 / 255.0 && over * 1000 <= before.data.len() * 3);
        if let Ok(dir) = std::env::var("EC_SNAPSHOT_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            for (name, img) in [("live", before), ("frozen", after)] {
                image::save_buffer(format!("{dir}/freeze-{three_d}-{name}.png"), &img.to_rgba8(), img.width, img.height, image::ColorType::Rgba8).unwrap();
            }
        }
        std::fs::remove_dir_all(effectcraft_engine::freeze_storage::root(&s.project.freeze.namespace).unwrap()).unwrap();
    }
}
