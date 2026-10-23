//! Layer Styles pixel tests.

use effectcraft_color::Label;
use effectcraft_keyframe::{Gradient, Keyframe, ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::styles::{self as st, GROUP};
use effectcraft_project::{Comp, ItemId, ItemKind, Layer, LayerSource, MaskMode, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{Image, NoFootage, RenderOpts, Renderer, render_frame};

const NORMAL: u32 = 0;
const MULTIPLY: u32 = 3;

fn setup() -> (Project, ItemId, Comp) {
    let mut p = Project::default();
    let comp = Comp::new(200, 100, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    (p, cid, comp)
}

fn solid(p: &mut Project, comp: &Comp, color: [f32; 3], w: u32, h: u32) -> Layer {
    let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color, width: w, height: h, pixel_aspect: 1.0 }));
    build::layer(p, comp, "Solid", LayerSource::Solid { item: sid }, (w, h), None)
}

fn add(p: &mut Project, comp: &Comp, l: &mut Layer, style: &str) {
    let mut next = p.next_id;
    st::add_style(l, &mut Ids(&mut next), style, &comp.global_light, true).unwrap();
    p.next_id = next;
}

fn set(l: &mut Layer, path: &str, v: Value) {
    l.props.prop_mut(&format!("{GROUP}/{path}")).unwrap_or_else(|| panic!("no {path}")).value = v;
}

/// A 40×40 square of `color` centred in a 200×100 comp (x 80..120, y 30..70) over an optional
/// full-frame background.
fn scene(color: [f32; 3], bg: Option<[f32; 3]>, f: impl FnOnce(&mut Project, &Comp, &mut Layer)) -> (Project, ItemId) {
    let (mut p, cid, comp) = setup();
    let mut l = solid(&mut p, &comp, color, 40, 40);
    f(&mut p, &comp, &mut l);
    let mut layers = vec![l];
    if let Some(bg) = bg {
        layers.push(solid(&mut p, &comp, bg, 200, 100));
    }
    p.comp_mut(cid).unwrap().layers = layers.into();
    (p, cid)
}

fn render(p: &Project, cid: ItemId) -> Image {
    render_frame(p, cid, Tick::ZERO, 1.0)
}

fn near(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn drop_shadow_offset_colour_and_opacity() {
    let (p, cid) = scene([1.0; 3], Some([0.5; 3]), |p, c, l| {
        add(p, c, l, "dropShadow");
        set(l, "dropShadow/size", Value::Scalar(0.0));
        set(l, "dropShadow/distance", Value::Scalar(10.0));
        set(l, "dropShadow/opacity", Value::Scalar(100.0));
        set(l, "dropShadow/blendMode", Value::Enum(NORMAL));
    });
    let img = render(&p, cid);
    // Global light 120° → the shadow falls down-right by (5, 8.66).
    assert!(near(img.get(123, 75)[0], 0.0, 0.02), "shadow: {:?}", img.get(123, 75));
    assert!(near(img.get(82, 75)[0], 0.5, 0.01), "left of the shadow stays grey");
    assert!(near(img.get(100, 50)[0], 1.0, 0.01), "layer on top");
    assert!(near(img.get(100, 20)[0], 0.5, 0.01), "above the layer");

    // 75 % Multiply (the default mode) over grey: 0.5 × (1 − 0.75).
    let (p, cid) = scene([1.0; 3], Some([0.5; 3]), |p, c, l| {
        add(p, c, l, "dropShadow");
        set(l, "dropShadow/size", Value::Scalar(0.0));
        set(l, "dropShadow/distance", Value::Scalar(10.0));
        set(l, "dropShadow/color", Value::Color([0.0, 0.0, 0.0, 1.0]));
    });
    let img = render(&p, cid);
    assert!(near(img.get(123, 75)[0], 0.125, 0.02), "{:?}", img.get(123, 75));

    // Coloured shadow on a transparent comp extends the layer's bounds.
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "dropShadow");
        set(l, "dropShadow/size", Value::Scalar(0.0));
        set(l, "dropShadow/distance", Value::Scalar(10.0));
        set(l, "dropShadow/opacity", Value::Scalar(50.0));
        set(l, "dropShadow/blendMode", Value::Enum(NORMAL));
        set(l, "dropShadow/color", Value::Color([0.0, 0.0, 1.0, 1.0]));
    });
    let img = render(&p, cid);
    let px = img.straight(123, 75);
    assert!(near(px[3], 0.5, 0.02) && near(px[2], 1.0, 0.01) && px[0] < 0.01, "{px:?}");
}

#[test]
fn soft_shadow_falls_off() {
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "dropShadow");
        set(l, "dropShadow/size", Value::Scalar(10.0));
        set(l, "dropShadow/distance", Value::Scalar(0.0));
        set(l, "dropShadow/opacity", Value::Scalar(100.0));
    });
    let img = render(&p, cid);
    let a1 = img.get(121, 50)[3];
    let a5 = img.get(125, 50)[3];
    let a20 = img.get(140, 50)[3];
    assert!(a1 > a5 && a5 > a20, "{a1} {a5} {a20}");
    assert!(a20 < 0.01);
}

#[test]
fn stroke_positions() {
    let render_stroke = |pos: u32| {
        let (p, cid) = scene([1.0; 3], None, |p, c, l| {
            add(p, c, l, "stroke");
            set(l, "stroke/size", Value::Scalar(4.0));
            set(l, "stroke/position", Value::Enum(pos));
        });
        render(&p, cid)
    };
    let red = |px: [f32; 4]| px[0] > 0.98 && px[1] < 0.02 && px[3] > 0.98;
    let white = |px: [f32; 4]| px[0] > 0.98 && px[1] > 0.98 && px[3] > 0.98;
    // Outside: 4 px ring beyond the right edge (x = 120).
    let o = render_stroke(0);
    assert!(red(o.get(122, 50)) && red(o.get(123, 50)), "{:?}", o.get(122, 50));
    assert!(o.get(126, 50)[3] < 0.01);
    assert!(white(o.get(118, 50)));
    // Inside: the ring is within the layer.
    let i = render_stroke(1);
    assert!(red(i.get(118, 50)) && red(i.get(117, 50)));
    assert!(white(i.get(110, 50)));
    assert!(i.get(122, 50)[3] < 0.01);
    // Centre: 2 px on each side.
    let c = render_stroke(2);
    assert!(red(c.get(119, 50)) && red(c.get(120, 50)) && red(c.get(121, 50)));
    assert!(white(c.get(116, 50)));
    assert!(c.get(124, 50)[3] < 0.01);
}

#[test]
fn colour_overlay_blend_modes() {
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "colorOverlay");
        set(l, "colorOverlay/color", Value::Color([0.0, 0.0, 1.0, 1.0]));
        set(l, "colorOverlay/opacity", Value::Scalar(50.0));
    });
    let px = render(&p, cid).get(100, 50);
    assert!(near(px[0], 0.5, 0.01) && near(px[2], 1.0, 0.01), "{px:?}");

    let (p, cid) = scene([0.5, 1.0, 1.0], None, |p, c, l| {
        add(p, c, l, "colorOverlay");
        set(l, "colorOverlay/color", Value::Color([1.0, 0.5, 0.0, 1.0]));
        set(l, "colorOverlay/blendMode", Value::Enum(MULTIPLY));
    });
    let img = render(&p, cid);
    let px = img.get(100, 50);
    assert!(near(px[0], 0.5, 0.01) && near(px[1], 0.5, 0.01) && near(px[2], 0.0, 0.01), "{px:?}");
    // Overlays stay inside the layer.
    assert!(img.get(130, 50)[3] < 0.01);
}

#[test]
fn gradient_overlay_follows_angle() {
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "gradientOverlay");
    });
    let img = render(&p, cid);
    // 90°: black at the bottom, white at the top.
    let top = img.get(100, 31)[0];
    let bottom = img.get(100, 68)[0];
    assert!(top > 0.9 && bottom < 0.1, "{top} {bottom}");
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "gradientOverlay");
        set(l, "gradientOverlay/angle", Value::Scalar(0.0));
        set(l, "gradientOverlay/reverse", Value::Bool(true));
    });
    let img = render(&p, cid);
    assert!(img.get(81, 50)[0] > 0.9 && img.get(118, 50)[0] < 0.1);
}

/// Overlays replace the layer's colour inside its alpha: an anti-aliased edge pixel mixes the
/// background with the overlay colour only, exactly as a layer of that colour would, with no
/// fringe of the layer's own colour (#416).
#[test]
fn overlays_leave_antialiased_edges_clean() {
    let (blue, bg) = ([0.2, 0.533, 1.0], [0.25, 0.22, 0.45]);
    // A mask half a pixel inside the square's left and right sides: in layer space, where the
    // styles run, columns 80 and 119 are half covered.
    let edge = |color: [f32; 3], f: &dyn Fn(&mut Project, &Comp, &mut Layer)| {
        let (p, cid) = scene(color, Some(bg), |p, c, l| {
            let mut next = p.next_id;
            let pts = vec![[0.5, 0.0], [39.5, 0.0], [39.5, 40.0], [0.5, 40.0]];
            let sp = ShapePath { in_tangents: vec![[0.0; 2]; 4], out_tangents: vec![[0.0; 2]; 4], vertices: pts, closed: true, feather: Vec::new() };
            let m = build::mask(&mut Ids(&mut next), "Mask 1", sp, MaskMode::Add, [255, 255, 0]);
            p.next_id = next;
            l.props.group_mut("masks").unwrap().children.push(m.into());
            f(p, c, l);
        });
        let img = render(&p, cid);
        [img.get(80, 50), img.get(119, 50), img.get(100, 50)]
    };
    let plain = edge(blue, &|_, _, _| {});
    assert!(near(plain[0][0], 0.5 * (blue[0] + bg[0]), 0.01), "half covered: {:?}", plain[0]);
    let color = Value::Color([blue[0] as f64, blue[1] as f64, blue[2] as f64, 1.0]);
    let flat = Gradient {
        colors: vec![(0.0, [blue[0], blue[1], blue[2], 1.0]), (1.0, [blue[0], blue[1], blue[2], 1.0])],
        opacities: vec![(0.0, 1.0), (1.0, 1.0)],
        ..Default::default()
    };
    let styled: [(&str, Box<dyn Fn(&mut Project, &Comp, &mut Layer)>); 3] = [
        (
            "Color Overlay",
            Box::new(|p, c, l| {
                add(p, c, l, "colorOverlay");
                set(l, "colorOverlay/color", color.clone());
            }),
        ),
        (
            "Gradient Overlay",
            Box::new(|p, c, l| {
                add(p, c, l, "gradientOverlay");
                set(l, "gradientOverlay/colors", Value::gradient(flat.clone()));
            }),
        ),
        (
            "Stroke (inside)",
            Box::new(|p, c, l| {
                add(p, c, l, "stroke");
                set(l, "stroke/position", Value::Enum(1));
                set(l, "stroke/size", Value::Scalar(25.0));
                set(l, "stroke/color", color.clone());
            }),
        ),
    ];
    for (name, f) in &styled {
        let px = edge([1.0; 3], f.as_ref());
        for (a, b) in px.iter().zip(&plain) {
            for ch in 0..4 {
                assert!(near(a[ch], b[ch], 0.01), "{name}: {a:?} against the plain layer {b:?}");
            }
        }
    }
}

fn bevel_scene(angle: f64) -> Image {
    let (p, cid) = scene([0.5; 3], None, |p, c, l| {
        add(p, c, l, "bevelEmboss");
        set(l, "bevelEmboss/size", Value::Scalar(6.0));
        set(l, "blendingOptions/globalLightAngle", Value::Scalar(angle));
    });
    render(&p, cid)
}

#[test]
fn bevel_lights_the_side_facing_the_light() {
    // Light from the upper left (120°): left and top edges brighten, right and bottom darken.
    let img = bevel_scene(120.0);
    let (l, r, t, b, mid) = (img.get(82, 50)[0], img.get(117, 50)[0], img.get(100, 32)[0], img.get(100, 67)[0], img.get(100, 50)[0]);
    assert!(l > 0.6 && t > 0.6, "highlights {l} {t}");
    assert!(r < 0.4 && b < 0.4, "shadows {r} {b}");
    assert!(near(mid, 0.5, 0.02), "flat centre {mid}");
    // Light from the lower right (−60°) swaps them.
    let img = bevel_scene(-60.0);
    assert!(img.get(82, 50)[0] < 0.4 && img.get(117, 50)[0] > 0.6);
}

#[test]
fn global_light_drives_linked_styles() {
    let shadow_at = |use_global: bool| {
        let (p, cid) = scene([1.0; 3], None, |p, c, l| {
            add(p, c, l, "dropShadow");
            set(l, "dropShadow/size", Value::Scalar(0.0));
            set(l, "dropShadow/distance", Value::Scalar(10.0));
            set(l, "dropShadow/opacity", Value::Scalar(100.0));
            set(l, "dropShadow/useGlobalLight", Value::Bool(use_global));
            set(l, "dropShadow/angle", Value::Scalar(90.0));
            set(l, "blendingOptions/globalLightAngle", Value::Scalar(0.0));
        });
        render(&p, cid)
    };
    // Global light at 0° (from the right): shadow to the left.
    let g = shadow_at(true);
    assert!(g.get(75, 50)[3] > 0.98 && g.get(100, 75)[3] < 0.01);
    // Own angle 90° (from above): shadow below.
    let o = shadow_at(false);
    assert!(o.get(100, 75)[3] > 0.98 && o.get(75, 50)[3] < 0.01);
}

#[test]
fn glows_and_inner_shadow() {
    let (p, cid) = scene([0.0, 0.0, 0.0], None, |p, c, l| {
        add(p, c, l, "outerGlow");
        add(p, c, l, "innerGlow");
        set(l, "outerGlow/opacity", Value::Scalar(100.0));
        set(l, "outerGlow/size", Value::Scalar(8.0));
        set(l, "innerGlow/opacity", Value::Scalar(100.0));
        set(l, "innerGlow/size", Value::Scalar(8.0));
    });
    let img = render(&p, cid);
    let out_near = img.straight(121, 50);
    assert!(out_near[3] > 0.6 && out_near[0] > 0.9 && out_near[2] < 0.9, "glow colour {out_near:?}");
    assert!(img.get(140, 50)[3] < 0.02, "glow fades");
    let edge = img.get(119, 50);
    let centre = img.get(100, 50);
    assert!(edge[0] > 0.6 && centre[0] < 0.05, "inner glow from the edge {edge:?} {centre:?}");

    // Inner glow from the centre instead.
    let (p, cid) = scene([0.0; 3], None, |p, c, l| {
        add(p, c, l, "innerGlow");
        set(l, "innerGlow/source", Value::Enum(0));
        set(l, "innerGlow/opacity", Value::Scalar(100.0));
    });
    let img = render(&p, cid);
    assert!(img.get(100, 50)[0] > 0.9 && img.get(119, 50)[0] < 0.6);

    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "innerShadow");
        set(l, "innerShadow/size", Value::Scalar(0.0));
        set(l, "innerShadow/opacity", Value::Scalar(100.0));
        set(l, "innerShadow/blendMode", Value::Enum(NORMAL));
    });
    let img = render(&p, cid);
    // Light from the upper left: the shadow lines the top-left inside edges.
    assert!(img.get(81, 50)[0] < 0.05 && img.get(100, 32)[0] < 0.05, "{:?}", img.get(81, 50));
    assert!(img.get(117, 50)[0] > 0.95 && img.get(100, 66)[0] > 0.95);
    assert!(img.get(76, 50)[3] < 0.01, "inner shadow stays inside");
}

#[test]
fn satin_shades_inside() {
    let (p, cid) = scene([1.0; 3], None, |p, c, l| add(p, c, l, "satin"));
    let img = render(&p, cid);
    let vals: Vec<f32> = (82..118).map(|x| img.get(x, 50)[0]).collect();
    let min = vals.iter().cloned().fold(1.0, f32::min);
    assert!(min < 0.9, "satin darkens somewhere: {min}");
    assert!(img.get(130, 50)[3] < 0.01);
}

#[test]
fn fill_opacity_knockout_and_channels() {
    // Fill 0: the content disappears but the stroke stays.
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "stroke");
        set(l, "blendingOptions/advancedBlending/fillOpacity", Value::Scalar(0.0));
    });
    let img = render(&p, cid);
    assert!(img.get(100, 50)[3] < 0.01 && img.get(122, 50)[0] > 0.98);

    // Interior styles ignore Fill Opacity unless blended as a group.
    let overlay = |group: bool| {
        let (p, cid) = scene([1.0; 3], None, |p, c, l| {
            add(p, c, l, "colorOverlay");
            set(l, "blendingOptions/advancedBlending/fillOpacity", Value::Scalar(0.0));
            set(l, "blendingOptions/advancedBlending/blendInteriorAsGroup", Value::Bool(group));
        });
        render(&p, cid).get(100, 50)
    };
    assert!(overlay(false)[3] > 0.98 && overlay(true)[3] < 0.01);

    // Knockout with Fill 0 punches through the background.
    let (p, cid) = scene([1.0; 3], Some([0.5; 3]), |p, c, l| {
        add(p, c, l, "stroke");
        set(l, "blendingOptions/advancedBlending/fillOpacity", Value::Scalar(0.0));
        set(l, "blendingOptions/advancedBlending/knockout", Value::Enum(2));
    });
    let img = render(&p, cid);
    assert!(img.get(100, 50)[3] < 0.01 && img.get(150, 50)[3] > 0.99);

    // Red channel off: the white layer leaves red from below.
    let (p, cid) = scene([1.0; 3], Some([0.0; 3]), |p, c, l| {
        add(p, c, l, "stroke");
        set(l, "blendingOptions/advancedBlending/red", Value::Bool(false));
    });
    let px = render(&p, cid).get(100, 50);
    assert!(px[0] < 0.01 && px[1] > 0.99 && px[2] > 0.99, "{px:?}");
}

#[test]
fn eye_switch_and_group_switch() {
    let (mut p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "stroke");
    });
    assert!(render(&p, cid).get(122, 50)[3] > 0.98);
    p.comp_mut(cid).unwrap().layers[0].props.group_mut(&format!("{GROUP}/stroke")).unwrap().enabled = false;
    assert!(render(&p, cid).get(122, 50)[3] < 0.01);
    p.comp_mut(cid).unwrap().layers[0].props.group_mut(&format!("{GROUP}/stroke")).unwrap().enabled = true;
    p.comp_mut(cid).unwrap().layers[0].props.group_mut(GROUP).unwrap().enabled = false;
    assert!(render(&p, cid).get(122, 50)[3] < 0.01);
}

#[test]
fn styles_respect_render_scale() {
    let (p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "dropShadow");
        add(p, c, l, "stroke");
        set(l, "dropShadow/size", Value::Scalar(0.0));
        set(l, "dropShadow/distance", Value::Scalar(20.0));
        set(l, "dropShadow/opacity", Value::Scalar(100.0));
        set(l, "stroke/size", Value::Scalar(4.0));
    });
    let full = render_frame(&p, cid, Tick::ZERO, 1.0);
    let half = render_frame(&p, cid, Tick::ZERO, 0.5);
    // Shadow offset (10, 17.3): at half resolution the shadow sits at half the distance.
    assert!(full.get(125, 85)[3] > 0.98);
    assert!(half.get(62, 42)[3] > 0.98 && half.get(55, 25)[3] > 0.98);
    // Stroke 4 px → 2 px at half.
    assert!(half.get(61, 25)[0] > 0.98 && half.get(61, 25)[1] < 0.05);
    assert!(half.get(64, 25)[3] < 0.01 || half.get(64, 25)[1] < 0.05);
}

#[test]
fn animated_style_properties() {
    let (mut p, cid) = scene([1.0; 3], None, |p, c, l| add(p, c, l, "stroke"));
    let l = &mut p.comp_mut(cid).unwrap().layers[0];
    l.props.prop_mut(&format!("{GROUP}/stroke/size")).unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Scalar(2.0)), Keyframe::new(Tick::from_seconds_f64(1.0), Value::Scalar(10.0))];
    let a = render_frame(&p, cid, Tick::ZERO, 1.0);
    let b = render_frame(&p, cid, Tick::from_seconds_f64(1.0), 1.0);
    assert!(a.get(126, 50)[3] < 0.01 && b.get(126, 50)[3] > 0.98);
}

fn render_cached(p: &Project, cid: ItemId, cache: &crate::LayerCache) -> Image {
    let mut r = Renderer::new(p, &NoFootage, RenderOpts::default());
    r.cache = Some(cache);
    r.comp_frame(cid, Tick::ZERO)
}

#[test]
fn style_edits_invalidate_the_cache() {
    let (mut p, cid) = scene([1.0; 3], Some([0.5; 3]), |p, c, l| {
        add(p, c, l, "dropShadow");
        add(p, c, l, "stroke");
    });
    let cache = crate::LayerCache::default();
    let a = render_cached(&p, cid, &cache);
    assert_eq!(a.data, render(&p, cid).data);
    let again = render_cached(&p, cid, &cache);
    assert_eq!(a.data, again.data);
    // Change a style value: the styled layer re-renders, its content (and the background) come
    // from the cache.
    let hits0 = cache.stats().hits;
    let l = &mut p.comp_mut(cid).unwrap().layers[0];
    l.props.prop_mut(&format!("{GROUP}/stroke/color")).unwrap().value = Value::Color([0.0, 1.0, 0.0, 1.0]);
    let b = render_cached(&p, cid, &cache);
    assert_eq!(b.data, render(&p, cid).data);
    assert_ne!(a.data, b.data);
    assert!(cache.stats().hits >= hits0 + 2, "content + background reused");
    // Global light change (linked) also invalidates.
    let l = &mut p.comp_mut(cid).unwrap().layers[0];
    l.props.prop_mut(&format!("{GROUP}/blendingOptions/globalLightAngle")).unwrap().value = Value::Scalar(0.0);
    let c = render_cached(&p, cid, &cache);
    assert_eq!(c.data, render(&p, cid).data);
    assert_ne!(b.data, c.data);
    // Eye switch off.
    p.comp_mut(cid).unwrap().layers[0].props.group_mut(&format!("{GROUP}/dropShadow")).unwrap().enabled = false;
    let d = render_cached(&p, cid, &cache);
    assert_eq!(d.data, render(&p, cid).data);
    assert_ne!(c.data, d.data);
}

#[test]
fn gradient_glow_uses_gradient() {
    let (p, cid) = scene([0.0; 3], None, |p, c, l| {
        add(p, c, l, "outerGlow");
        set(l, "outerGlow/colorType", Value::Enum(1));
        set(l, "outerGlow/opacity", Value::Scalar(100.0));
        set(
            l,
            "outerGlow/colors",
            Value::gradient(Gradient {
                colors: vec![(0.0, [1.0, 0.0, 0.0, 1.0]), (1.0, [0.0, 0.0, 1.0, 1.0])],
                opacities: vec![(0.0, 1.0), (1.0, 0.0)],
                ..Default::default()
            }),
        );
    });
    let img = render(&p, cid);
    let px = img.straight(121, 50);
    assert!(px[0] > px[2], "near the edge the glow takes the gradient's start colour {px:?}");
}

/// A track matte reads its layer before Layer Styles: SceneYard "Apple Maps Route" mattes its map
/// through a rounded rectangle with Fill Opacity 0 and a Stroke style (the matte's own look is
/// just the ring). After Effects shows the whole map inside it; we showed only the ring.
#[test]
fn track_matte_reads_the_layer_before_its_styles() {
    let (mut p, cid, comp) = setup();
    let mut matte = solid(&mut p, &comp, [1.0; 3], 40, 40);
    add(&mut p, &comp, &mut matte, "stroke");
    set(&mut matte, "stroke/size", Value::Scalar(4.0));
    set(&mut matte, "blendingOptions/advancedBlending/fillOpacity", Value::Scalar(0.0));
    let mut fill = solid(&mut p, &comp, [0.0, 0.0, 1.0], 200, 100);
    fill.track_matte = Some(effectcraft_project::TrackMatte { layer: matte.id, kind: effectcraft_project::MatteKind::Alpha });
    p.comp_mut(cid).unwrap().layers = vec![matte, fill].into();
    let img = render(&p, cid);
    // Inside the square (x 80..120): the blue layer, not just the ring.
    assert!(img.get(100, 50)[2] > 0.98 && img.get(100, 50)[3] > 0.98, "{:?}", img.get(100, 50));
    assert!(img.get(10, 10)[3] < 0.01);
}

#[test]
fn repeated_drop_shadows_render_both_sides_and_respect_enabled_state() {
    let (mut p, cid) = scene([1.0; 3], None, |p, c, l| {
        add(p, c, l, "dropShadow");
        set(l, "dropShadow/useGlobalLight", Value::Bool(false));
        set(l, "dropShadow/angle", Value::Scalar(0.0));
        set(l, "dropShadow/distance", Value::Scalar(30.0));
        set(l, "dropShadow/size", Value::Scalar(0.0));
        set(l, "dropShadow/color", Value::Color([1.0, 0.0, 0.0, 1.0]));
        set(l, "dropShadow/opacity", Value::Scalar(100.0));
        set(l, "dropShadow/blendMode", Value::Enum(NORMAL));
        let mut next = p.next_id;
        let mut second = st::style(&mut Ids(&mut next), "dropShadow").unwrap();
        for (key, value) in [
            ("useGlobalLight", Value::Bool(false)),
            ("angle", Value::Scalar(180.0)),
            ("distance", Value::Scalar(30.0)),
            ("size", Value::Scalar(0.0)),
            ("color", Value::Color([0.0, 0.0, 1.0, 1.0])),
            ("opacity", Value::Scalar(100.0)),
            ("blendMode", Value::Enum(NORMAL)),
        ] {
            second.get_mut(key).unwrap().value = value;
        }
        p.next_id = next;
        l.props.sub_mut(GROUP).unwrap().children.push(second.into());
    });
    let img = render(&p, cid);
    assert!(img.get(60, 50)[0] > 0.99 && img.get(60, 50)[3] > 0.99);
    assert!(img.get(140, 50)[2] > 0.99 && img.get(140, 50)[3] > 0.99);
    let ls = p.comp_mut(cid).unwrap().layers[0].props.sub_mut(GROUP).unwrap();
    let uid = ls.groups().filter(|g| g.match_id == "dropShadow").last().unwrap().uid;
    ls.find_group_mut(uid).unwrap().enabled = false;
    let img = render(&p, cid);
    assert!(img.get(60, 50)[0] > 0.99);
    assert_eq!(img.get(140, 50)[3], 0.0);
}

/// Styles run on the content's visible part only (cropped to its alpha, then padded by their
/// reach): the same pixels as styling the whole mostly transparent buffer a shape layer with
/// effects is drawn on, noise, Dissolve and jitter grain included, within 1/255.
#[test]
fn cropped_styles_match_the_whole_buffer() {
    const DISSOLVE: u32 = 1;
    let sets: [&[(&str, &[(&str, Value)])]; 2] = [
        &[
            ("dropShadow", &[("size", Value::Scalar(9.0)), ("distance", Value::Scalar(6.0)), ("noise", Value::Scalar(30.0))]),
            ("outerGlow", &[("size", Value::Scalar(12.0)), ("noise", Value::Scalar(20.0)), ("jitter", Value::Scalar(40.0)), ("colorType", Value::Enum(1))]),
            ("innerShadow", &[("size", Value::Scalar(5.0)), ("noise", Value::Scalar(10.0))]),
            ("satin", &[]),
            // Dissolve on an interior style: its grain is the buffer's (an exterior pass is
            // dissolved by the compositor, in comp pixels).
            ("colorOverlay", &[("opacity", Value::Scalar(40.0)), ("blendMode", Value::Enum(DISSOLVE))]),
            ("gradientOverlay", &[("opacity", Value::Scalar(60.0)), ("alignWithLayer", Value::Bool(true))]),
        ],
        &[
            ("innerGlow", &[("technique", Value::Enum(1)), ("source", Value::Enum(0)), ("noise", Value::Scalar(15.0))]),
            ("stroke", &[("size", Value::Scalar(4.0)), ("position", Value::Enum(2))]),
            ("bevelEmboss", &[("size", Value::Scalar(8.0)), ("soften", Value::Scalar(2.0))]),
            ("dropShadow", &[("spread", Value::Scalar(40.0)), ("size", Value::Scalar(10.0))]),
        ],
    ];
    // A soft-edged blob in a 300×200 transparent canvas (a comp-sized effect canvas).
    let (cw, ch) = (300u32, 200u32);
    let mut img = Image::new(cw, ch);
    for y in 0..ch {
        for x in 0..cw {
            let (dx, dy) = ((x as f32 - 140.0) / 22.0, (y as f32 - 95.0) / 15.0);
            let a = (1.6 - (dx * dx + dy * dy).sqrt() * 1.2).clamp(0.0, 1.0);
            img.data[(y * cw + x) as usize] = [0.9 * a, 0.4 * a, 0.2 * a, a];
        }
    }
    let content = effectcraft_effects::Buf { img, offset: [150.0, 100.0], scale: 1.0 };
    for styles in sets {
        let (mut p, cid, comp) = setup();
        let mut l = solid(&mut p, &comp, [1.0; 3], 40, 40);
        for (style, params) in styles {
            add(&mut p, &comp, &mut l, style);
            for (k, v) in *params {
                set(&mut l, &format!("{style}/{k}"), v.clone());
            }
        }
        let ctx = crate::eval::EvalCtx::new(&p, cid, &comp, Tick::ZERO);
        let whole = crate::styles::render_in(&ctx, &l, &content, [40.0, 40.0], false).flatten();
        let cropped = crate::styles::render_in(&ctx, &l, &content, [40.0, 40.0], true).flatten();
        assert!(cropped.img.width * 2 < whole.img.width && cropped.img.height * 2 < whole.img.height, "not cropped");
        let (dx, dy) = (whole.offset[0] - cropped.offset[0], whole.offset[1] - cropped.offset[1]);
        assert!(dx.fract() == 0.0 && dy.fract() == 0.0 && dx >= 0.0 && dy >= 0.0, "{dx} {dy}");
        let mut worst = (0.0f32, 0, 0);
        for y in 0..whole.img.height {
            for x in 0..whole.img.width {
                let a = whole.img.get(x as i64, y as i64);
                let (cx, cy) = (x as i64 - dx as i64, y as i64 - dy as i64);
                let inside = cx >= 0 && cy >= 0 && (cx as u32) < cropped.img.width && (cy as u32) < cropped.img.height;
                let b = if inside { cropped.img.get(cx, cy) } else { [0.0; 4] };
                for c in 0..4 {
                    if (a[c] - b[c]).abs() > worst.0 {
                        worst = ((a[c] - b[c]).abs(), x, y);
                    }
                }
            }
        }
        assert!(worst.0 <= 1.0 / 255.0, "{:?}: max difference {worst:?}", styles.iter().map(|s| s.0).collect::<Vec<_>>());
    }
}

/// A text layer with an effect is a comp-sized canvas; its styles run on the visible part
/// (`layer_buf::crop_visible`). The frame must be exactly the one styled on the whole canvas,
/// for every style, at Full and Half, and a style hashed by buffer pixel (noise) keeps the
/// whole canvas.
#[test]
fn styles_on_a_cropped_text_canvas_match_the_whole_canvas() {
    use crate::layer_buf::{NO_STYLE_CROP, STYLE_CROPS};
    use effectcraft_keyframe::TextDoc;
    use std::sync::atomic::Ordering;
    type Setup = fn(&mut Project, &Comp, &mut Layer);
    let setups: [(&str, Setup); 10] = [
        ("drop", |p, c, l| {
            add(p, c, l, "dropShadow");
            set(l, "dropShadow/size", Value::Scalar(12.0));
            set(l, "dropShadow/spread", Value::Scalar(20.0));
        }),
        ("outer glow", |p, c, l| {
            add(p, c, l, "outerGlow");
            set(l, "outerGlow/size", Value::Scalar(8.0));
        }),
        ("precise glow", |p, c, l| {
            add(p, c, l, "outerGlow");
            set(l, "outerGlow/technique", Value::Enum(1));
            set(l, "outerGlow/size", Value::Scalar(15.0));
        }),
        ("inner glow + shadow", |p, c, l| {
            add(p, c, l, "innerGlow");
            add(p, c, l, "innerShadow");
        }),
        ("bevel", |p, c, l| add(p, c, l, "bevelEmboss")),
        ("satin", |p, c, l| add(p, c, l, "satin")),
        ("stroke", |p, c, l| {
            add(p, c, l, "stroke");
            set(l, "stroke/size", Value::Scalar(6.0));
        }),
        ("Comp 29 counter", |p, c, l| {
            add(p, c, l, "outerGlow");
            set(l, "outerGlow/size", Value::Scalar(8.0));
            add(p, c, l, "gradientOverlay");
        }),
        ("gradient, not aligned", |p, c, l| {
            add(p, c, l, "gradientOverlay");
            set(l, "gradientOverlay/alignWithLayer", Value::Bool(false));
            add(p, c, l, "colorOverlay");
            set(l, "colorOverlay/opacity", Value::Scalar(40.0));
        }),
        ("noisy glow", |p, c, l| {
            add(p, c, l, "outerGlow");
            set(l, "outerGlow/noise", Value::Scalar(30.0));
        }),
    ];
    for (name, f) in setups {
        let mut p = Project::default();
        let comp = Comp::new(400, 200, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut l = build::layer(&mut p, &comp, "Text", LayerSource::Text, (400, 200), None);
        l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(TextDoc { text: "$165".into(), size: 40.0, ..Default::default() }));
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([150.7, 90.3, 0.0]);
        l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([103.0, 103.0, 100.0]);
        // A Transform effect that moves the text gives it a comp-sized effect canvas.
        let spec = effectcraft_effects::find("ec.distort.transform").unwrap();
        let mut next = p.next_id;
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [400.0, 200.0]);
        p.next_id = next;
        g.prop_mut("position").unwrap().value = Value::Vec2([210.0, 105.0]);
        l.props.sub_mut("effects").unwrap().children.push(g.into());
        f(&mut p, &comp, &mut l);
        let bg = solid(&mut p, &comp, [0.3, 0.4, 0.5], 400, 200);
        p.comp_mut(cid).unwrap().layers = vec![l, bg].into();
        for scale in [1.0, 0.5] {
            NO_STYLE_CROP.store(true, Ordering::Relaxed);
            let whole = render_frame(&p, cid, Tick::ZERO, scale);
            NO_STYLE_CROP.store(false, Ordering::Relaxed);
            let before = STYLE_CROPS.load(Ordering::Relaxed);
            let cropped = render_frame(&p, cid, Tick::ZERO, scale);
            let crops = STYLE_CROPS.load(Ordering::Relaxed) - before;
            if name == "noisy glow" {
                assert_eq!(crops, 0, "{name}: noise is hashed by buffer pixel, no crop");
            } else {
                assert!(crops > 0, "{name} at {scale}: styles ran on the whole canvas");
            }
            let diff = whole.data.iter().zip(&cropped.data).filter(|(a, b)| a != b).count();
            assert_eq!(diff, 0, "{name} at {scale}: {diff} pixels differ");
            assert!(whole.data.iter().any(|px| px[0] > 0.31 || px[0] < 0.29), "{name}: the text is drawn");
        }
    }
}

/// A Transform effect that only fades (Anchor Point = Position, Scale 100 %, no Rotation or
/// Skew) leaves a text layer on its tight glyph buffer instead of the comp-sized effect canvas,
/// with the frame the canvas gives; one that moves pixels keeps the canvas.
#[test]
fn a_fading_transform_effect_keeps_text_on_its_glyph_buffer() {
    use crate::fx::ALWAYS_TEXT_CANVAS;
    use crate::{EvalCtx, NoFootage, RenderOpts, Renderer};
    use effectcraft_keyframe::TextDoc;
    use std::sync::atomic::Ordering;
    for (rotation, styled) in [(0.0, false), (0.0, true), (12.0, false)] {
        let mut p = Project::default();
        let comp = Comp::new(400, 200, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut l = build::layer(&mut p, &comp, "Text", LayerSource::Text, (400, 200), None);
        l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(TextDoc { text: "$165".into(), size: 40.0, ..Default::default() }));
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([150.7, 90.3, 0.0]);
        l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([103.0, 103.0, 100.0]);
        let spec = effectcraft_effects::find("ec.distort.transform").unwrap();
        let mut next = p.next_id;
        let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [400.0, 200.0]);
        p.next_id = next;
        g.prop_mut("opacity").unwrap().value = Value::Scalar(60.0);
        g.prop_mut("rotation").unwrap().value = Value::Scalar(rotation);
        l.props.sub_mut("effects").unwrap().children.push(g.into());
        if styled {
            add(&mut p, &comp, &mut l, "outerGlow");
            add(&mut p, &comp, &mut l, "gradientOverlay");
        }
        let bg = solid(&mut p, &comp, [0.3, 0.4, 0.5], 400, 200);
        p.comp_mut(cid).unwrap().layers = vec![l, bg].into();
        let c = p.comp(cid).unwrap();
        let layer = c.layers.iter().next().unwrap();
        let width = |scale: f64| {
            let r = Renderer::new(&p, &NoFootage, RenderOpts { scale, ..Default::default() });
            r.content_buf(&EvalCtx::new(&p, cid, c, Tick::ZERO), layer).unwrap().img.width
        };
        for scale in [1.0, 0.5] {
            ALWAYS_TEXT_CANVAS.store(true, Ordering::Relaxed);
            let canvas = render_frame(&p, cid, Tick::ZERO, scale);
            let wide = width(scale);
            ALWAYS_TEXT_CANVAS.store(false, Ordering::Relaxed);
            let tight = render_frame(&p, cid, Tick::ZERO, scale);
            let narrow = width(scale);
            let comp_w = (400.0 * scale) as u32;
            if rotation == 0.0 {
                assert!(narrow < comp_w / 2 && wide >= comp_w, "fade only, styled {styled}, at {scale}: {narrow} vs {wide} px wide");
            } else {
                assert_eq!(narrow, wide, "a rotating Transform keeps the canvas");
            }
            let worst = canvas.data.iter().zip(&tight.data).flat_map(|(a, b)| (0..4).map(move |i| (a[i] - b[i]).abs())).fold(0.0f32, f32::max);
            assert!(worst <= 1.0 / 255.0, "rotation {rotation}, styled {styled}, at {scale}: differs by {worst}");
        }
    }
}

/// Blurs, Glow and CC Radial Fast Blur on a text layer run on the part of the comp-sized effect
/// canvas they can reach (`Renderer::canvas_support`), with the frame the whole canvas gives:
/// 2D and a 3D Continuously Rasterized layer at a raster scale over 1 (P01 Clock's circle).
/// The canvas tests flip [`crate::fx::WHOLE_TEXT_CANVAS`]: one at a time.
static CANVAS_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn blurs_and_glows_run_on_the_reachable_part_of_the_text_canvas() {
    let _serial = CANVAS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use crate::fx::WHOLE_TEXT_CANVAS;
    use crate::{EvalCtx, NoFootage, RenderOpts, Renderer};
    use effectcraft_keyframe::TextDoc;
    use std::sync::atomic::Ordering;
    for (three_d, radial) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut p = Project::default();
        let comp = Comp::new(400, 200, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut l = build::layer(&mut p, &comp, "Text", LayerSource::Text, (400, 200), None);
        l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(TextDoc { text: "O".into(), size: 30.0, ..Default::default() }));
        l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([150.7, 90.3, 0.0]);
        if three_d {
            l.switches.three_d = true;
            l.switches.collapse = true;
            l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([180.0, 180.0, 100.0]);
        }
        let mut next = p.next_id;
        for (id, set) in [
            ("ec.blur.gaussian", vec![("blurriness", Value::Scalar(3.0)), ("repeatEdge", Value::Bool(true))]),
            ("ec.blur.ccradialfast", vec![("center", Value::Vec2([170.0, 110.0])), ("amount", Value::Scalar(60.0))]),
            ("ec.stylize.glow", vec![("radius", Value::Scalar(14.0)), ("threshold", Value::Scalar(40.0))]),
            ("ec.stylize.glow", vec![("radius", Value::Scalar(6.0)), ("based", Value::Enum(0))]),
        ] {
            if id == "ec.blur.ccradialfast" && !radial {
                continue;
            }
            let spec = effectcraft_effects::find(id).unwrap();
            let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [400.0, 200.0]);
            for (k, v) in set {
                g.prop_mut(k).unwrap().value = v;
            }
            l.props.sub_mut("effects").unwrap().children.push(g.into());
        }
        p.next_id = next;
        let bg = solid(&mut p, &comp, [0.3, 0.4, 0.5], 400, 200);
        p.comp_mut(cid).unwrap().layers = vec![l, bg].into();
        let c = p.comp(cid).unwrap();
        let layer = c.layers.iter().next().unwrap();
        let area = |scale: f64| {
            let r = Renderer::new(&p, &NoFootage, RenderOpts { scale, ..Default::default() });
            let b = r.content_buf(&EvalCtx::new(&p, cid, c, Tick::ZERO), layer).unwrap();
            b.img.width as u64 * b.img.height as u64
        };
        for scale in [1.0, 0.5] {
            WHOLE_TEXT_CANVAS.store(true, Ordering::Relaxed);
            let whole = render_frame(&p, cid, Tick::ZERO, scale);
            let big = area(scale);
            WHOLE_TEXT_CANVAS.store(false, Ordering::Relaxed);
            let part = render_frame(&p, cid, Tick::ZERO, scale);
            let small = area(scale);
            // With CC Radial Fast Blur the canvas keeps its top-left corner (`canvas_support`).
            let shrunk = if radial { small < big } else { small * 3 < big * 2 };
            assert!(shrunk, "3D {three_d}, radial {radial}, at {scale}: {small} vs {big} px");
            let worst = whole.data.iter().zip(&part.data).flat_map(|(a, b)| (0..4).map(move |i| (a[i] - b[i]).abs())).fold(0.0f32, f32::max);
            assert!(worst <= 1.0 / 255.0, "3D {three_d}, radial {radial}, at {scale}: differs by {worst}");
        }
    }
}

/// Drop Shadow, Deep Glow, Bevel Alpha and Fill on a text or shape layer run on the part of the
/// comp-sized effect canvas they can reach (`Renderer::canvas_support`), with the frame the whole
/// canvas gives: each alone and stacked as on 10-10's captions (Deep Glow, two Drop Shadows,
/// Bevel Alpha), 2D and a 3D Continuously Rasterized layer scaled up.
#[test]
fn shadows_deep_glow_and_bevel_run_on_the_reachable_part_of_the_canvas() {
    let _serial = CANVAS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use crate::fx::WHOLE_TEXT_CANVAS;
    use crate::{EvalCtx, NoFootage, RenderOpts, Renderer};
    use effectcraft_keyframe::TextDoc;
    use std::sync::atomic::Ordering;
    let shadow = |d: f64, s: f64| ("ec.perspective.dropshadow", vec![("distance", Value::Scalar(d)), ("softness", Value::Scalar(s))]);
    let glow = ("ec.stylize.deepglow", vec![("PEDG-0001", Value::Scalar(24.0))]);
    let bevel = ("ec.perspective.bevelalpha", vec![("edgeThickness", Value::Scalar(3.0))]);
    let fill = ("ec.generate.fill", vec![("color", Value::Color([0.2, 0.9, 0.4, 1.0]))]);
    let stacks = [vec![shadow(12.0, 20.0)], vec![glow.clone()], vec![bevel.clone(), fill.clone()], vec![glow, shadow(4.0, 9.0), shadow(0.0, 30.0), bevel]];
    for (n, stack) in stacks.iter().enumerate() {
        for (text, three_d) in [(true, false), (true, true), (false, false), (false, true)] {
            let mut p = Project::default();
            let comp = Comp::new(400, 200, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
            let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
            let src = if text { LayerSource::Text } else { LayerSource::Shape };
            let mut l = build::layer(&mut p, &comp, "L", src, (400, 200), None);
            let mut next = p.next_id;
            if text {
                l.props.prop_mut("text/sourceText").unwrap().value = Value::Text(Box::new(TextDoc { text: "Ok".into(), size: 30.0, ..Default::default() }));
            } else {
                let mut ids = Ids(&mut next);
                let items = vec![build::shape_rect(&mut ids, [40.0, 26.0], [0.0, 0.0], 6.0), build::shape_fill(&mut ids, [0.9, 0.6, 0.2, 1.0])];
                let g = build::shape_group(&mut ids, "R", items);
                l.props.sub_mut("contents").unwrap().children.push(g.into());
            }
            l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([150.7, 90.3, 0.0]);
            if three_d {
                l.switches.three_d = true;
                l.switches.collapse = true;
                l.props.prop_mut("transform/scale").unwrap().value = Value::Vec3([180.0, 180.0, 100.0]);
            }
            for (id, set) in stack {
                let spec = effectcraft_effects::find(id).unwrap();
                let mut g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [400.0, 200.0]);
                for (k, v) in set {
                    g.prop_mut(k).unwrap().value = v.clone();
                }
                l.props.sub_mut("effects").unwrap().children.push(g.into());
            }
            p.next_id = next;
            let bg = solid(&mut p, &comp, [0.3, 0.4, 0.5], 400, 200);
            p.comp_mut(cid).unwrap().layers = vec![l, bg].into();
            let c = p.comp(cid).unwrap();
            let layer = c.layers.iter().next().unwrap();
            let area = |scale: f64| {
                let r = Renderer::new(&p, &NoFootage, RenderOpts { scale, ..Default::default() });
                let b = r.content_buf(&EvalCtx::new(&p, cid, c, Tick::ZERO), layer).unwrap();
                b.img.width as u64 * b.img.height as u64
            };
            for scale in [1.0, 0.5] {
                let at = format!("stack {n}, text {text}, 3D {three_d}, at {scale}");
                WHOLE_TEXT_CANVAS.store(true, Ordering::Relaxed);
                let whole = render_frame(&p, cid, Tick::ZERO, scale);
                let big = area(scale);
                WHOLE_TEXT_CANVAS.store(false, Ordering::Relaxed);
                let part = render_frame(&p, cid, Tick::ZERO, scale);
                let small = area(scale);
                // The whole caption stack reaches most of this small comp.
                let shrunk = if stack.len() > 2 { small < big } else { small * 3 < big * 2 };
                assert!(shrunk, "{at}: {small} vs {big} px");
                // One 8-bit step at most (the float sums round either way). Deep Glow's
                // Downsample pass resamples the whole canvas and its Dither follows buffer
                // pixels, so its halo may move by two steps on a few pixels.
                let deep = stack.iter().any(|(id, _)| *id == "ec.stylize.deepglow");
                let (steps, share) = if deep { (2.0, 100) } else { (1.0, 1000) };
                let diffs: Vec<f32> = whole.data.iter().zip(&part.data).flat_map(|(a, b)| (0..4).map(move |i| (a[i] - b[i]).abs())).collect();
                let worst = diffs.iter().copied().fold(0.0f32, f32::max);
                let off = diffs.iter().filter(|d| **d > 1e-6).count();
                assert!(worst <= steps / 255.0 + 1e-6 && off * share <= diffs.len(), "{at}: differs by {worst} on {off} of {} channels", diffs.len());
            }
        }
    }
}
