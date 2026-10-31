//! In context: a scene comp seen where it sits in MAIN, with the scenes above it left out.

use effectcraft_color::Label;
use effectcraft_project::build;
use effectcraft_project::{Comp, ItemId, ItemKind, LayerSource, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{ContextChain, NoFootage, RenderOpts, Renderer};

/// MAIN (100×100), top to bottom: scene B (a green square at the bottom right), scene A (a red
/// one at the top left), a BG precomp R under A (a white square at the bottom left) and a
/// full-frame blue solid (the presenter). Returns (project, MAIN, A's precomp layer).
fn video() -> (Project, ItemId, effectcraft_project::LayerId) {
    let mut p = Project::default();
    let t = Tick::from_seconds_f64(2.0);
    let scene = |p: &mut Project, name: &str, color: [f32; 3], at: (f64, f64)| {
        let mut c = Comp::new(100, 100, FrameRate::FPS_30, t);
        let sid = p.add_item(name, Label::Red, None, ItemKind::Solid(Solid { color, width: 20, height: 20, pixel_aspect: 1.0 }));
        let mut l = build::layer(p, &c, name, LayerSource::Solid { item: sid }, (20, 20), None);
        l.props.prop_mut("transform/position").unwrap().value = effectcraft_keyframe::Value::Vec3([at.0, at.1, 0.0]);
        c.layers.push(l);
        p.add_item(name, Label::Sandstone, None, ItemKind::Comp(c.into()))
    };
    let a = scene(&mut p, "A", [1.0, 0.0, 0.0], (20.0, 20.0));
    let b = scene(&mut p, "B", [0.0, 1.0, 0.0], (80.0, 80.0));
    let r = scene(&mut p, "R", [1.0, 1.0, 1.0], (20.0, 80.0));
    let main = Comp::new(100, 100, FrameRate::FPS_30, t);
    let bg = p.add_item("Presenter", Label::Red, None, ItemKind::Solid(Solid { color: [0.0, 0.0, 1.0], width: 100, height: 100, pixel_aspect: 1.0 }));
    let la = build::layer(&mut p, &main, "A", LayerSource::Comp { item: a }, (100, 100), None);
    let lb = build::layer(&mut p, &main, "B", LayerSource::Comp { item: b }, (100, 100), None);
    let lr = build::layer(&mut p, &main, "R", LayerSource::Comp { item: r }, (100, 100), None);
    let lbg = build::layer(&mut p, &main, "Presenter", LayerSource::Solid { item: bg }, (100, 100), None);
    let a_layer = la.id;
    let mid = p.add_item("MAIN", Label::Sandstone, None, ItemKind::Comp(main.into()));
    for l in [lb, la, lr, lbg] {
        p.comp_mut(mid).unwrap().layers.push(l);
    }
    (p, mid, a_layer)
}

fn px(img: &effectcraft_raster::Image, x: usize, y: usize) -> [f32; 4] {
    img.data[y * img.width as usize + x]
}

#[test]
fn in_context_a_scene_shows_over_main_without_the_scenes_above_it() {
    let (p, main, a) = video();
    let plain = Renderer::new(&p, &NoFootage, RenderOpts::default()).comp_frame(main, Tick::ZERO);
    assert!(px(&plain, 80, 80)[1] > 0.9, "MAIN shows scene B");
    let opts = RenderOpts { context: ContextChain::new(&[(main, a)]), ..Default::default() };
    let ctx = Renderer::new(&p, &NoFootage, opts).comp_frame(main, Tick::ZERO);
    assert!(px(&ctx, 20, 20)[0] > 0.9, "scene A is drawn");
    assert!(px(&ctx, 80, 80)[2] > 0.9 && px(&ctx, 80, 80)[1] < 0.1, "scene B is left out; the presenter shows there");
    assert!(px(&ctx, 50, 50)[2] > 0.9, "the presenter stays under the scene");
    assert!(px(&ctx, 20, 80)[1] > 0.9 && px(&ctx, 20, 80)[2] > 0.9, "the BG precomp under the scene stays");
    assert!(format!("{opts:?}").contains("context"), "the context keys the caches");
    assert!(!format!("{:?}", RenderOpts::default()).contains("context"), "other renders keep their keys");
}
