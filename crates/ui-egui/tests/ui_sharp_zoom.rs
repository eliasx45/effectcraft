//! View ▸ Sharp Zoom: above 100 % the viewer draws the visible region rendered at the
//! magnification over the magnified comp-resolution frame (text crisp at 400 %); off, or at
//! 100 %, only the comp-resolution frame shows.

use effectcraft_engine::Session;
use effectcraft_engine::render::{NoFootage, RenderOpts, Renderer};
use effectcraft_engine::time::Tick;
use effectcraft_ui_egui::EffectcraftApp;
use egui_kittest::Harness;
use serde_json::json;

fn harness(zoom: f32, sharp: bool) -> Harness<'static, EffectcraftApp> {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Zoom", "width": 640, "height": 360, "duration": 4})).unwrap();
    s.execute("layer.newText", json!({"text": "VIRAL", "font": "Inter", "size": 48, "position": [320, 196]})).unwrap();
    s.execute("edit.deselectAll", json!({})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 1000.0)).with_pixels_per_point(1.0).build_eframe(move |_| {
        let mut app = EffectcraftApp::new(s);
        app.ui.viewer.zoom = Some(zoom);
        app.ui.viewer.sharp_zoom = sharp;
        app
    });
    h.run_steps(3);
    h
}

/// Step the viewer until a sharp tile is on screen (the tile renders on its own thread).
fn wait_for_tile(h: &mut Harness<'_, EffectcraftApp>) -> bool {
    for _ in 0..400 {
        h.step();
        if h.state().auto.find_current("viewer.sharpZoom").is_some() && !h.state().sharp_zoom_busy() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Share of the drawn pixels that are partly covered (edge width, see the renderer's tests).
fn soft_share(px: impl Iterator<Item = u8> + Clone) -> f64 {
    let drawn = px.clone().filter(|a| *a > 12).count();
    let soft = px.filter(|a| *a > 12 && *a < 243).count();
    soft as f64 / drawn.max(1) as f64
}

#[test]
fn text_at_400_percent_is_sharper_than_the_magnified_frame() {
    let mut h = harness(4.0, true);
    assert!(wait_for_tile(&mut h), "no sharp tile at 400 %");
    let (key, tile) = h.state().sharp_zoom_tile().unwrap();
    assert_eq!(key.scale(), 4.0);
    let [x, y, w, hh] = key.region;
    // The visible part of the comp, whole output pixels at 4×.
    assert!(x >= 0.0 && y >= 0.0 && x + w <= 640.0 && y + hh <= 360.0, "{:?}", key.region);
    assert!(x < 320.0 && x + w > 320.0 && y < 180.0 && y + hh > 180.0, "covers the centre: {:?}", key.region);
    assert_eq!(tile.size, [(w * 4.0).round() as usize, (hh * 4.0).round() as usize]);
    // The magnified path: the comp-resolution frame, each comp pixel a 4×4 block.
    let s = &h.state().session;
    let cid = s.active_comp_id().unwrap();
    let full = Renderer::new(&s.project, &NoFootage, RenderOpts { guides: true, ..Default::default() }).comp_frame(cid, Tick::ZERO);
    let (x0, y0) = (x as i64, y as i64);
    let magnified = (0..tile.size[1]).flat_map(|ty| (0..tile.size[0]).map(move |tx| (tx, ty))).map(|(tx, ty)| {
        let p = full.get(x0 + tx as i64 / 4, y0 + ty as i64 / 4);
        (p[3].clamp(0.0, 1.0) * 255.0).round() as u8
    });
    let sharp = tile.pixels.iter().map(|c| c.a());
    let covered = |px: &mut dyn Iterator<Item = u8>| px.map(f64::from).sum::<f64>() / 255.0;
    let (a, b) = (covered(&mut sharp.clone()), covered(&mut magnified.clone()));
    assert!(a > 1000.0 && (a - b).abs() < 0.1 * b, "the same glyphs: coverage {a} vs {b}");
    let (s, m) = (soft_share(sharp), soft_share(magnified));
    assert!(s < 0.5 * m, "edges: sharp {s:.3} vs magnified {m:.3}");
}

#[test]
fn no_tile_when_off_or_at_100_percent() {
    let mut h = harness(4.0, false);
    assert!(!h.state().ui.viewer.sharp_zoom);
    for _ in 0..30 {
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(h.state().auto.find_current("viewer.sharpZoom").is_none(), "Sharp Zoom off: the magnified frame only");
    assert!(h.state().sharp_zoom_tile().is_none(), "nothing rendered while off");

    // The menu command turns it back on (the default).
    let ctx = h.ctx.clone();
    effectcraft_ui_egui::menus::frontend(h.state_mut(), &ctx, "view.sharpZoom", json!({})).unwrap();
    assert!(h.state().ui.viewer.sharp_zoom);

    let mut h = harness(1.0, true);
    for _ in 0..30 {
        h.step();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(h.state().sharp_zoom_tile().is_none() && !h.state().sharp_zoom_busy(), "100 %: the comp-resolution frame is exact");
}
