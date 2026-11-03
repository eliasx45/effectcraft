use effectcraft_color::Label;
use effectcraft_geom::{Vec3, vec3};
use effectcraft_keyframe::Value;
use effectcraft_project::build;
use effectcraft_project::{Comp, ItemId, ItemKind, Layer, LayerSource, LightKind, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use super::camera::{Rig, View3D, default_view_cam, orientation_for};
use super::*;
use crate::{NoFootage, RenderOpts, Renderer, render_frame};

const W: u32 = 200;
const H: u32 = 100;

fn setup() -> (Project, ItemId, Comp) {
    let mut p = Project::default();
    // Exact float maths; 8/16 bpc quantisation has its own tests (tests_color).
    p.settings.bit_depth = effectcraft_project::BitDepth::Bpc32;
    let comp = Comp::new(W, H, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    (p, cid, comp)
}

/// A 3D solid of `w`×`h` centred at `pos`.
fn solid3(p: &mut Project, comp: &Comp, color: [f32; 3], w: u32, h: u32, pos: [f64; 3]) -> Layer {
    let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color, width: w, height: h, pixel_aspect: 1.0 }));
    let mut l = build::layer(p, comp, "Solid", LayerSource::Solid { item: sid }, (w, h), None);
    l.switches.three_d = true;
    set(&mut l, "transform/position", Value::Vec3(pos));
    l
}

fn set(l: &mut Layer, path: &str, v: Value) {
    l.props.prop_mut(path).unwrap_or_else(|| panic!("no {path}")).value = v;
}

fn push(p: &mut Project, cid: ItemId, l: Layer) {
    // Appended = lowest in the stack.
    p.comp_mut(cid).unwrap().layers.push(l);
}

fn render(p: &Project, cid: ItemId) -> effectcraft_raster::Image {
    render_frame(p, cid, Tick::ZERO, 1.0)
}

fn close(a: f32, b: f32, eps: f32) -> bool {
    (a - b).abs() <= eps
}

fn zoom() -> f64 {
    W as f64 * 50.0 / 36.0
}

// ---------------------------------------------------------------- projection

#[test]
fn z0_plane_matches_2d() {
    let (mut p, cid, comp) = setup();
    let l = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 40, 20, [50.0, 30.0, 0.0]);
    push(&mut p, cid, l);
    let img = render(&p, cid);
    // Covers x 30..70, y 20..40.
    assert_eq!(img.get(31, 21), [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(img.get(68, 38), [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(img.get(29, 30)[3], 0.0);
    assert_eq!(img.get(71, 30)[3], 0.0);
}

#[test]
fn farther_layers_project_smaller() {
    let (mut p, cid, comp) = setup();
    // At z = zoom the plane is twice as far from the eye: half size around the comp centre.
    let l = solid3(&mut p, &comp, [0.0, 1.0, 0.0], 80, 40, [100.0, 50.0, zoom()]);
    push(&mut p, cid, l);
    let img = render(&p, cid);
    // 40×20 centred on (100, 50): x 80..120, y 40..60.
    assert!(img.get(82, 42)[3] > 0.99);
    assert!(img.get(117, 57)[3] > 0.99);
    assert!(img.get(77, 50)[3] < 0.01, "{:?}", img.get(77, 50));
    assert!(img.get(100, 37)[3] < 0.01);
}

#[test]
fn projection_math_matches_camera_state() {
    let cam = default_camera(1920.0, 1080.0);
    let pr = cam.projection(1920.0, 1080.0);
    let p = pr.apply(vec3(100.0, 200.0, 0.0));
    assert!((p.x - 100.0).abs() < 1e-6 && (p.y - 200.0).abs() < 1e-6);
    // A point at depth 2·zoom maps halfway to the centre.
    let q = cam.project(1920.0, 1080.0, vec3(0.0, 0.0, cam.zoom)).unwrap();
    assert!((q.x - 480.0).abs() < 1e-6 && (q.y - 270.0).abs() < 1e-6, "{q:?}");
    assert!(cam.project(1920.0, 1080.0, vec3(960.0, 540.0, -cam.zoom - 10.0)).is_none());
}

#[test]
fn y_rotation_foreshortens_with_correct_perspective() {
    let (mut p, cid, comp) = setup();
    let mut l = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 100, 60, [100.0, 50.0, 0.0]);
    set(&mut l, "transform/rotationY", Value::Scalar(60.0));
    push(&mut p, cid, l);
    let img = render(&p, cid);
    // cos 60° = 0.5: roughly half as wide; the near edge (right, z<0) is taller than the far edge.
    assert!(img.get(100, 50)[3] > 0.99);
    assert!(img.get(140, 50)[3] < 0.01);
    let col_h = |x: u32| (0..H).filter(|&y| img.get(x as i64, y as i64)[3] > 0.5).count();
    assert!(col_h(122) > col_h(78), "near edge {} vs far edge {}", col_h(122), col_h(78));
}

#[test]
fn layer_behind_camera_is_clipped() {
    let (mut p, cid, comp) = setup();
    let l = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 200, 100, [100.0, 50.0, -zoom() - 50.0]);
    push(&mut p, cid, l);
    let img = render(&p, cid);
    assert!(img.data.iter().all(|px| px[3] == 0.0));
}

// ---------------------------------------------------------------- depth ordering

#[test]
fn nearer_layer_wins_regardless_of_stack_order() {
    let (mut p, cid, comp) = setup();
    let far = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 600, 300, [100.0, 50.0, 200.0]);
    let near = solid3(&mut p, &comp, [0.0, 1.0, 0.0], 40, 40, [100.0, 50.0, 0.0]);
    // Far layer is on top of the stack.
    push(&mut p, cid, far);
    push(&mut p, cid, near);
    let img = render(&p, cid);
    assert_eq!(img.get(100, 50), [0.0, 1.0, 0.0, 1.0]);
    assert_eq!(img.get(5, 50)[0], 1.0);
}

#[test]
fn coplanar_layers_keep_stack_order() {
    let (mut p, cid, comp) = setup();
    let top = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 40, 40, [100.0, 50.0, 30.0]);
    let bottom = solid3(&mut p, &comp, [0.0, 0.0, 1.0], 80, 80, [100.0, 50.0, 30.0]);
    push(&mut p, cid, top);
    push(&mut p, cid, bottom);
    let img = render(&p, cid);
    assert_eq!(img.get(100, 50), [1.0, 0.0, 0.0, 1.0]);
}

#[test]
fn intersecting_planes_split_per_pixel() {
    let (mut p, cid, comp) = setup();
    let flat = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 160, 80, [100.0, 50.0, 0.0]);
    let mut tilted = solid3(&mut p, &comp, [0.0, 1.0, 0.0], 160, 80, [100.0, 50.0, 0.0]);
    // Rotated about Y: its right half comes towards the camera, its left half goes behind.
    set(&mut tilted, "transform/rotationY", Value::Scalar(40.0));
    push(&mut p, cid, tilted);
    push(&mut p, cid, flat);
    let img = render(&p, cid);
    assert_eq!(img.get(125, 50), [0.0, 1.0, 0.0, 1.0], "right: tilted plane in front");
    assert_eq!(img.get(75, 50), [1.0, 0.0, 0.0, 1.0], "left: flat plane in front");
}

#[test]
fn two_d_layer_breaks_3d_group() {
    let (mut p, cid, comp) = setup();
    let far_top = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 60, 60, [100.0, 50.0, 300.0]);
    let sid = p.add_item("S2", Label::Red, None, ItemKind::Solid(Solid { color: [0.0, 0.0, 1.0], width: 20, height: 100, pixel_aspect: 1.0 }));
    let mut two_d = build::layer(&mut p, &comp, "2D", LayerSource::Solid { item: sid }, (20, 100), None);
    set(&mut two_d, "transform/position", Value::Vec3([100.0, 50.0, 0.0]));
    let near_bottom = solid3(&mut p, &comp, [0.0, 1.0, 0.0], 200, 100, [100.0, 50.0, 0.0]);
    // Stack: far 3D (top), 2D, near 3D (bottom). The far layer is a separate group above the
    // 2D layer, so it draws over everything even though it is farther away.
    push(&mut p, cid, far_top);
    push(&mut p, cid, two_d);
    push(&mut p, cid, near_bottom);
    let img = render(&p, cid);
    assert_eq!(img.get(100, 50), [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(img.get(5, 5), [0.0, 1.0, 0.0, 1.0]);
}

// ---------------------------------------------------------------- lighting

fn light(p: &mut Project, comp: &Comp, kind: LightKind, pos: [f64; 3]) -> Layer {
    let mut l = build::layer(p, comp, "Light", LayerSource::Light { kind }, (W, H), None);
    if kind != LightKind::Ambient {
        set(&mut l, "transform/position", Value::Vec3(pos));
    }
    l
}

fn no_specular(l: &mut Layer) {
    set(l, "materialOptions/specularIntensity", Value::Scalar(0.0));
}

#[test]
fn head_on_point_light_scales_by_diffuse() {
    let (mut p, cid, comp) = setup();
    let mut s = solid3(&mut p, &comp, [1.0, 0.8, 0.6], 200, 100, [100.0, 50.0, 0.0]);
    no_specular(&mut s);
    let l = light(&mut p, &comp, LightKind::Point, [100.0, 50.0, -500.0]);
    push(&mut p, cid, l);
    push(&mut p, cid, s);
    let img = render(&p, cid);
    let c = img.get(100, 50);
    // Diffuse 50% · N·L = 1 · intensity 100%.
    assert!(close(c[0], 0.5, 1e-3) && close(c[1], 0.4, 1e-3) && close(c[2], 0.3, 1e-3), "{c:?}");
    // Off-axis pixels are lit at a grazing angle → darker.
    assert!(img.get(2, 2)[0] < c[0]);
}

#[test]
fn ambient_light_uses_ambient_coefficient() {
    let (mut p, cid, comp) = setup();
    let mut s = solid3(&mut p, &comp, [0.8, 0.8, 0.8], 200, 100, [100.0, 50.0, 0.0]);
    set(&mut s, "materialOptions/ambient", Value::Scalar(50.0));
    let mut l = light(&mut p, &comp, LightKind::Ambient, [0.0; 3]);
    set(&mut l, "lightOptions/intensity", Value::Scalar(50.0));
    push(&mut p, cid, l);
    push(&mut p, cid, s);
    let c = render(&p, cid).get(10, 10);
    assert!(close(c[0], 0.8 * 0.5 * 0.5, 1e-4), "{c:?}");
}

#[test]
fn lights_off_or_not_accepting_lights_render_unlit() {
    let (mut p, cid, comp) = setup();
    let mut s = solid3(&mut p, &comp, [0.3, 0.6, 0.9], 200, 100, [100.0, 50.0, 0.0]);
    set(&mut s, "materialOptions/acceptsLights", Value::Bool(false));
    let l = light(&mut p, &comp, LightKind::Point, [100.0, 50.0, -500.0]);
    push(&mut p, cid, l);
    push(&mut p, cid, s);
    let c = render(&p, cid).get(100, 50);
    assert!(close(c[0], 0.3, 1e-5) && close(c[2], 0.9, 1e-5), "{c:?}");
}

#[test]
fn light_behind_layer_needs_transmission() {
    let (mut p, cid, comp) = setup();
    let mut s = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 200, 100, [100.0, 50.0, 0.0]);
    no_specular(&mut s);
    let l = light(&mut p, &comp, LightKind::Point, [100.0, 50.0, 500.0]);
    push(&mut p, cid, l);
    push(&mut p, cid, s.clone());
    assert!(render(&p, cid).get(100, 50)[0] < 1e-4);
    // 100% transmission passes the light through the layer.
    let c = p.comp_mut(cid).unwrap();
    set(&mut c.layers[1], "materialOptions/lightTransmission", Value::Scalar(100.0));
    let v = render(&p, cid).get(100, 50)[0];
    assert!(close(v, 0.5, 1e-3), "{v}");
}

#[test]
fn specular_highlight_adds_light() {
    let (mut p, cid, comp) = setup();
    let s = solid3(&mut p, &comp, [0.5, 0.5, 0.5], 200, 100, [100.0, 50.0, 0.0]);
    let l = light(&mut p, &comp, LightKind::Point, [100.0, 50.0, -zoom()]);
    push(&mut p, cid, l);
    push(&mut p, cid, s);
    let c = render(&p, cid).get(100, 50);
    // 0.5·0.5 diffuse + 0.5 specular (N·H = 1) tinted by the layer colour (Metal 100%) = 0.5.
    assert!(close(c[0], 0.25 + 0.25, 2e-3), "{c:?}");
}

#[test]
fn spot_cone_limits_light() {
    let (mut p, cid, comp) = setup();
    let mut s = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 200, 100, [100.0, 50.0, 0.0]);
    no_specular(&mut s);
    let mut l = light(&mut p, &comp, LightKind::Spot, [100.0, 50.0, -200.0]);
    set(&mut l, "transform/poi", Value::Vec3([100.0, 50.0, 0.0]));
    set(&mut l, "lightOptions/coneAngle", Value::Scalar(20.0));
    set(&mut l, "lightOptions/coneFeather", Value::Scalar(0.0));
    push(&mut p, cid, l);
    push(&mut p, cid, s);
    let img = render(&p, cid);
    assert!(img.get(100, 50)[0] > 0.45);
    // tan(10°)·200 ≈ 35 px radius.
    assert!(img.get(160, 50)[0] < 1e-4);
}

#[test]
fn falloff_inverse_square_clamped() {
    let mut ls = LightState::from_layer(&crate::EvalCtx::new(&Project::default(), ItemId(0), &Comp::new(W, H, FrameRate::FPS_30, Tick::ZERO), Tick::ZERO), &{
        let (mut p, _, comp) = setup();
        light(&mut p, &comp, LightKind::Point, [0.0, 0.0, 0.0])
    })
    .unwrap();
    ls.falloff = 2;
    ls.radius = 100.0;
    assert!((ls.attenuation(vec3(50.0, 0.0, 0.0)) - 1.0).abs() < 1e-12);
    assert!((ls.attenuation(vec3(200.0, 0.0, 0.0)) - 0.25).abs() < 1e-12);
    ls.falloff = 1;
    ls.falloff_distance = 100.0;
    assert!((ls.attenuation(vec3(150.0, 0.0, 0.0)) - 0.5).abs() < 1e-12);
    assert_eq!(ls.attenuation(vec3(250.0, 0.0, 0.0)), 0.0);
}

// ---------------------------------------------------------------- shadows

#[test]
fn caster_shadows_receiver() {
    let (mut p, cid, comp) = setup();
    let mut recv = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 200, 100, [100.0, 50.0, 100.0]);
    no_specular(&mut recv);
    let mut caster = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 20, 20, [60.0, 50.0, 0.0]);
    no_specular(&mut caster);
    set(&mut caster, "materialOptions/castsShadows", Value::Enum(1));
    // Parallel light straight along +z: the shadow lands right behind the caster.
    let mut l = light(&mut p, &comp, LightKind::Parallel, [100.0, 50.0, -500.0]);
    set(&mut l, "transform/poi", Value::Vec3([100.0, 50.0, 0.0]));
    set(&mut l, "lightOptions/castsShadows", Value::Bool(true));
    push(&mut p, cid, l);
    push(&mut p, cid, caster);
    push(&mut p, cid, recv);
    let img = render(&p, cid);
    // Lit receiver away from the shadow: 0.5.
    assert!(close(img.get(150, 50)[0], 0.5, 1e-3));
    // Light from the left (direction (300, 0, 500)): the shadow shifts +60 px at the receiver's
    // depth, to world x 110..130 → screen ≈ 107..122.
    let c = p.comp_mut(cid).unwrap();
    set(&mut c.layers[0], "transform/position", Value::Vec3([100.0 - 300.0, 50.0, -500.0]));
    let img = render(&p, cid);
    let lit = img.get(150, 50)[0];
    let expect = 0.5 * (500.0 / (300.0f64 * 300.0 + 500.0 * 500.0).sqrt()) as f32;
    assert!(close(lit, expect, 1e-3), "lit {lit} vs {expect}");
    assert!(img.get(115, 50)[0] < 1e-3, "shadowed {:?}", img.get(115, 50));
    assert!(close(img.get(60, 50)[0], expect, 1e-3), "caster itself is lit");
}

#[test]
fn shadow_darkness_and_accepts_off() {
    let (mut p, cid, comp) = setup();
    let mut recv = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 400, 200, [100.0, 50.0, 100.0]);
    no_specular(&mut recv);
    let mut caster = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 400, 200, [100.0, 50.0, 0.0]);
    set(&mut caster, "materialOptions/castsShadows", Value::Enum(2)); // Only: invisible caster
    let mut l = light(&mut p, &comp, LightKind::Parallel, [100.0, 50.0, -500.0]);
    set(&mut l, "transform/poi", Value::Vec3([100.0, 50.0, 0.0]));
    set(&mut l, "lightOptions/castsShadows", Value::Bool(true));
    set(&mut l, "lightOptions/shadowDarkness", Value::Scalar(50.0));
    push(&mut p, cid, l);
    push(&mut p, cid, caster);
    push(&mut p, cid, recv);
    let v = render(&p, cid).get(100, 50)[0];
    assert!(close(v, 0.25, 1e-3), "half-dark shadow: {v}");
    let c = p.comp_mut(cid).unwrap();
    set(&mut c.layers[2], "materialOptions/acceptsShadows", Value::Enum(0));
    let v = render(&p, cid).get(100, 50)[0];
    assert!(close(v, 0.5, 1e-3), "no shadow accepted: {v}");
}

// ---------------------------------------------------------------- cameras

fn camera(p: &mut Project, comp: &Comp) -> Layer {
    build::layer(p, comp, "Camera", LayerSource::Camera, (W, H), None)
}

#[test]
fn default_camera_layer_matches_comp_camera() {
    let (mut p, cid, comp) = setup();
    let cam = camera(&mut p, &comp);
    assert_eq!(cam.props.prop("cameraOptions/zoom").unwrap().value.as_f64(), zoom());
    let s = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 40, 20, [50.0, 30.0, 0.0]);
    push(&mut p, cid, s.clone());
    let a = render(&p, cid);
    p.comp_mut(cid).unwrap().layers.insert(0, cam);
    let b = render(&p, cid);
    assert_eq!(a, b);
}

#[test]
fn two_node_camera_looks_at_poi() {
    let (mut p, cid, comp) = setup();
    let mut cam = camera(&mut p, &comp);
    // Look at a layer far to the right: it lands in the middle of the frame.
    set(&mut cam, "transform/poi", Value::Vec3([600.0, 50.0, 0.0]));
    set(&mut cam, "transform/position", Value::Vec3([600.0, 50.0, -zoom()]));
    let s = solid3(&mut p, &comp, [0.0, 0.0, 1.0], 20, 20, [600.0, 50.0, 0.0]);
    push(&mut p, cid, cam);
    push(&mut p, cid, s);
    let img = render(&p, cid);
    assert!(img.get(100, 50)[2] > 0.99);
    assert!(img.get(80, 50)[3] < 0.01);
}

#[test]
fn one_node_camera_uses_orientation() {
    let (mut p, cid, comp) = setup();
    let mut cam = camera(&mut p, &comp);
    cam.auto_orient = effectcraft_project::AutoOrient::Off;
    set(&mut cam, "transform/position", Value::Vec3([100.0, 50.0, 0.0]));
    // Turn to look along +x.
    set(&mut cam, "transform/orientation", Value::Vec3(orientation_for(vec3(1.0, 0.0, 0.0))));
    let mut s = solid3(&mut p, &comp, [1.0, 1.0, 0.0], 40, 40, [400.0, 50.0, 0.0]);
    set(&mut s, "transform/rotationY", Value::Scalar(90.0));
    push(&mut p, cid, cam);
    push(&mut p, cid, s);
    let img = render(&p, cid);
    assert!(img.get(100, 50)[3] > 0.99, "{:?}", img.get(100, 50));
}

#[test]
fn orientation_for_round_trips() {
    for d in [vec3(1.0, 0.0, 0.0), vec3(0.0, 0.0, 1.0), vec3(-0.3, 0.4, 0.8), vec3(0.2, -0.9, -0.1)] {
        let o = orientation_for(d);
        let f = effectcraft_geom::Mat4::orientation(Vec3::from(o)).apply_vec(vec3(0.0, 0.0, 1.0));
        assert!((f - d.normalize()).length() < 1e-9, "{d:?} → {o:?} → {f:?}");
    }
}

#[test]
fn depth_of_field_blurs_out_of_focus_layers() {
    let (mut p, cid, comp) = setup();
    let mut cam = camera(&mut p, &comp);
    set(&mut cam, "cameraOptions/dof", Value::Bool(true));
    set(&mut cam, "cameraOptions/aperture", Value::Scalar(60.0));
    let s = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 60, 60, [100.0, 50.0, 0.0]);
    push(&mut p, cid, cam);
    push(&mut p, cid, s);
    // In focus (focus distance = zoom = distance to z=0): hard edge at x = 70.
    let a = render(&p, cid);
    assert!(a.get(71, 50)[3] > 0.99 && a.get(69, 50)[3] < 0.01);
    let c = p.comp_mut(cid).unwrap();
    set(&mut c.layers[0], "cameraOptions/focusDistance", Value::Scalar(zoom() * 0.5));
    let b = render(&p, cid);
    let e = b.get(70, 50)[3];
    assert!(e > 0.1 && e < 0.9, "soft edge {e}");
    assert!(b.get(66, 50)[3] > 0.01);
}

/// A 3D layer scaled to 0 % under an out-of-focus camera (SceneYard "Digital Wallets" and its
/// finance scene folder): its blur radius in layer pixels was unbounded and the padding
/// overflowed. The plane is invisible; the rest of the scene still renders, CPU and accelerated.
#[test]
fn zero_scale_plane_under_depth_of_field_renders() {
    let (mut p, cid, comp) = setup();
    let mut cam = camera(&mut p, &comp);
    set(&mut cam, "cameraOptions/dof", Value::Bool(true));
    set(&mut cam, "cameraOptions/aperture", Value::Scalar(100.0));
    set(&mut cam, "cameraOptions/focusDistance", Value::Scalar(zoom() * 1.6));
    let mut gone = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 60, 20, [100.0, 50.0, -60.0]);
    set(&mut gone, "transform/scale", Value::Vec3([0.0, 0.0, 100.0]));
    let seen = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 40, 40, [100.0, 50.0, 0.0]);
    push(&mut p, cid, cam);
    push(&mut p, cid, seen);
    let without = render(&p, cid);
    p.comp_mut(cid).unwrap().layers.insert(1, gone);
    let img = render(&p, cid);
    assert!(img.get(100, 50)[3] > 0.3, "visible layer drawn: {:?}", img.get(100, 50));
    assert_eq!(img.data, without.data, "the degenerate plane adds nothing");
    // The accelerator's plan: the degenerate plane gets no blur plan, the visible one does.
    let c = p.comp(cid).unwrap();
    let ctx = crate::EvalCtx::new(&p, cid, c, Tick::ZERO);
    let r = Renderer::new(&p, &NoFootage, RenderOpts::default());
    let run: Vec<&Layer> = c.layers.iter().filter(|l| l.is_3d()).collect();
    let planned = r.prepare_3d_run(&ctx, &run, (W, H)).expect("accelerated run");
    assert!(planned.planes.iter().all(|pl| pl.dof.as_ref().is_none_or(|d| d.pad <= compose::MAX_DOF_REACH as u32 + 2)));
    assert!(planned.planes.iter().any(|pl| pl.dof.is_some()), "the visible plane still blurs");
}

/// A plane scaled almost to nothing magnifies its blur radius in layer pixels without limit;
/// the radius (and so the padded buffer) is bounded for the CPU blur and the accelerator alike.
#[test]
fn near_zero_scale_depth_of_field_is_bounded() {
    let (mut p, cid, comp) = setup();
    let mut cam = camera(&mut p, &comp);
    set(&mut cam, "cameraOptions/dof", Value::Bool(true));
    set(&mut cam, "cameraOptions/aperture", Value::Scalar(100.0));
    set(&mut cam, "cameraOptions/focusDistance", Value::Scalar(zoom() * 1.6));
    push(&mut p, cid, cam);
    let c = p.comp(cid).unwrap();
    let ctx = crate::EvalCtx::new(&p, cid, c, Tick::ZERO);
    let cs = super::camera::active_camera(&ctx);
    let dof = cs.dof.expect("depth of field on");
    for aspect in [1.0, 0.01, 100.0, f64::NAN] {
        let dof = super::camera::Dof { iris: super::bokeh::Iris { aspect, ..dof.iris }, ..dof };
        let world = effectcraft_geom::Mat4::scale(vec3(1e-6, 1e-6, 1.0));
        let radii = compose::dof_radii_at(&cs, &dof, world, (8, 8), [4.0, 4.0], 1.0);
        let rmax = radii.iter().copied().fold(0.0f32, f32::max) as f64;
        assert!(rmax > 0.3 && rmax.is_finite(), "still blurs: {rmax}");
        assert!(compose::dof_pad(&dof, rmax) <= compose::MAX_DOF_REACH as u32 + 2, "aspect {aspect}: rmax {rmax}");
    }
}

/// The circle of confusion is the thin lens': aperture · F / (focus − F) · |z − focus| / z,
/// with the focal length F in millimetres (Zoom on the 36 mm film) and distances in pixels, as
/// After Effects renders it. SceneYard "Digital Wallets" (1080-wide comp, Zoom 1866.67 = 62.2 mm,
/// Focus Distance 2573.67, Aperture 100, layers at depth 1648.67): After Effects shows them
/// sharp; the aperture alone made a 56-pixel circle and blurred everything.
#[test]
fn circle_of_confusion_is_the_thin_lens() {
    let (mut p, cid, _) = setup();
    let comp = Comp::new(1080, 1920, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
    let mut cam = camera(&mut p, &comp);
    set(&mut cam, "cameraOptions/zoom", Value::Scalar(1866.66666685));
    set(&mut cam, "cameraOptions/dof", Value::Bool(true));
    set(&mut cam, "cameraOptions/focusDistance", Value::Scalar(2573.66666685));
    set(&mut cam, "cameraOptions/aperture", Value::Scalar(100.0));
    let c = p.comp_mut(cid).unwrap();
    *c = comp.clone();
    c.layers.push(cam);
    let c = p.comp(cid).unwrap();
    let ctx = crate::EvalCtx::new(&p, cid, c, Tick::ZERO);
    let dof = super::camera::active_camera(&ctx).dof.expect("depth of field on");
    let f = 1866.66666685 * 36.0 / 1080.0;
    assert!((dof.focal_length - f).abs() < 1e-9, "{}", dof.focal_length);
    let z = 1648.66666685;
    let want = 100.0 * f / (2573.66666685 - f) * (z - 2573.66666685f64).abs() / z;
    assert!((dof.coc(z) - want).abs() < 1e-9 && dof.coc(z) < 1.5, "{} vs {want}", dof.coc(z));
    // Focus at or inside the focal length: bounded, finite.
    let near = super::camera::Dof { focus: f * 0.5, ..dof };
    assert!(near.coc(z).is_finite() && near.coc(z) <= 100.0 * f * (z - f * 0.5).abs() / z);
    // Unknown focal length: the aperture alone.
    let bare = super::camera::Dof { focal_length: 0.0, ..dof };
    assert!((bare.coc(z) - 100.0 * (z - 2573.66666685f64).abs() / z).abs() < 1e-9);
}

/// Pixels in column `x` whose alpha is strictly between 0.05 and 0.95.
fn soft_px(img: &effectcraft_raster::Image, x: i64) -> usize {
    (0..img.height as i64).filter(|&y| (0.05..0.95).contains(&img.get(x, y)[3])).count()
}

#[test]
fn tilted_layer_blurs_progressively_with_depth() {
    let (mut p, cid, comp) = setup();
    let mut cam = camera(&mut p, &comp);
    set(&mut cam, "cameraOptions/dof", Value::Bool(true));
    set(&mut cam, "cameraOptions/aperture", Value::Scalar(80.0));
    set(&mut cam, "cameraOptions/irisShape", Value::Enum(4)); // Hexagon
    let mut s = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 160, 40, [100.0, 50.0, 0.0]);
    set(&mut s, "transform/rotationY", Value::Scalar(60.0));
    push(&mut p, cid, cam);
    push(&mut p, cid, s);
    let img = render(&p, cid);
    // The centre column sits on the focal plane: a hard top edge. The ends are 70 px in front of
    // and behind it: soft.
    let xs: Vec<i64> = (0..W as i64).filter(|&x| img.get(x, 50)[3] > 0.5).collect();
    let (x0, x1) = (xs[0], *xs.last().unwrap());
    let at = |f: f64| x0 + ((x1 - x0) as f64 * f) as i64;
    let (centre, left, right) = (soft_px(&img, 100), soft_px(&img, at(0.1)), soft_px(&img, at(0.9)));
    assert!(centre <= 2, "centre {centre}");
    assert!(left >= 4 && right >= 4, "ends {left} {right}");
}

#[test]
fn iris_shape_and_highlights_shape_the_bokeh() {
    // A small bright square far out of focus: its blur takes the iris' shape.
    let spot = |shape: u32, gain: f64| {
        let (mut p, cid, comp) = setup();
        let mut cam = camera(&mut p, &comp);
        set(&mut cam, "cameraOptions/dof", Value::Bool(true));
        set(&mut cam, "cameraOptions/aperture", Value::Scalar(120.0));
        set(&mut cam, "cameraOptions/focusDistance", Value::Scalar(zoom() * 0.5));
        set(&mut cam, "cameraOptions/irisShape", Value::Enum(shape));
        set(&mut cam, "cameraOptions/highlightGain", Value::Scalar(gain));
        set(&mut cam, "cameraOptions/highlightThreshold", Value::Scalar(200.0));
        let s = solid3(&mut p, &comp, [1.0, 1.0, 1.0], 2, 2, [100.0, 50.0, 0.0]);
        push(&mut p, cid, cam);
        push(&mut p, cid, s);
        render(&p, cid)
    };
    let area = |img: &effectcraft_raster::Image| img.data.iter().filter(|q| q[3] > 1e-4).count();
    let (tri, dec, rect) = (spot(1, 0.0), spot(8, 0.0), spot(0, 0.0));
    assert!(area(&tri) * 3 < area(&dec) * 2, "triangle {} vs decagon {}", area(&tri), area(&dec));
    // Fast Rectangle: a box.
    let (x0, x1) = (0..W as i64).filter(|&x| rect.get(x, 50)[3] > 1e-4).fold((i64::MAX, 0), |(a, b), x| (a.min(x), b.max(x)));
    let y_extent = (0..H as i64).filter(|&y| rect.get(100, y)[3] > 1e-4).count() as i64;
    assert!(((x1 - x0 + 1) - y_extent).abs() <= 2, "square box {} vs {}", x1 - x0 + 1, y_extent);
    // Highlight Gain brightens the blurred highlight.
    let lit = spot(8, 100.0);
    let sum = |img: &effectcraft_raster::Image| img.data.iter().map(|q| q[0]).sum::<f32>();
    assert!(sum(&lit) > sum(&dec) * 1.5, "{} vs {}", sum(&lit), sum(&dec));
}

// ---------------------------------------------------------------- views

#[test]
fn view_ids_round_trip() {
    for v in View3D::ALL {
        assert_eq!(View3D::from_id(v.id()), Some(v));
        assert_eq!(View3D::from_id(v.label()), Some(v));
    }
    assert_eq!(View3D::from_id("Custom View 2"), Some(View3D::Custom2));
}

#[test]
fn front_view_is_orthographic() {
    let (mut p, cid, comp) = setup();
    // In an orthographic view depth doesn't change size.
    let a = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 40, 40, [60.0, 50.0, 0.0]);
    let b = solid3(&mut p, &comp, [0.0, 1.0, 0.0], 40, 40, [140.0, 50.0, 1000.0]);
    push(&mut p, cid, a);
    push(&mut p, cid, b);
    let mut vc = default_view_cam(View3D::Front, W as f64, H as f64);
    vc.zoom = 1.0;
    let opts = RenderOpts { view: Some(vc.state()), ..Default::default() };
    let img = Renderer::new(&p, &NoFootage, opts).comp_frame(cid, Tick::ZERO);
    assert!(img.get(41, 50)[0] > 0.99 && img.get(39, 50)[3] < 0.01);
    assert!(img.get(121, 50)[1] > 0.99 && img.get(159, 50)[1] > 0.99 && img.get(161, 50)[3] < 0.01);
}

#[test]
fn top_view_sees_planes_edge_on() {
    let (mut p, cid, comp) = setup();
    let a = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 100, 100, [100.0, 50.0, 0.0]);
    let mut b = solid3(&mut p, &comp, [0.0, 1.0, 0.0], 100, 100, [100.0, 50.0, 0.0]);
    set(&mut b, "transform/rotationX", Value::Scalar(90.0));
    push(&mut p, cid, a);
    push(&mut p, cid, b);
    let mut vc = default_view_cam(View3D::Top, W as f64, H as f64);
    vc.zoom = 1.0;
    let opts = RenderOpts { view: Some(vc.state()), ..Default::default() };
    let img = Renderer::new(&p, &NoFootage, opts).comp_frame(cid, Tick::ZERO);
    // From the top, the X-rotated plane is a 100×100 square; the upright one is a line.
    assert!(img.get(100, 30)[1] > 0.99 && img.get(100, 70)[1] > 0.99);
    assert!(img.get(100, 30)[0] < 0.01);
}

#[test]
fn rig_orbit_keeps_distance_and_pan_moves_both() {
    let vc = default_view_cam(View3D::Custom1, 1920.0, 1080.0);
    let mut r = Rig::from_view(&vc);
    let d0 = (r.eye - r.poi).length();
    r.orbit(37.0, -15.0);
    assert!(((r.eye - r.poi).length() - d0).abs() < 1e-6);
    let (e, p) = (r.eye, r.poi);
    r.pan(10.0, 0.0);
    assert!(((r.eye - e) - (r.poi - p)).length() < 1e-9);
    r.dolly(100.0, false);
    assert!(((r.eye - r.poi).length() - (d0 - 100.0)).abs() < 1e-6);
    // Pitch is clamped short of the poles.
    r.orbit(0.0, 500.0);
    let off = r.eye - r.poi;
    assert!(off.y.abs() / off.length() < 0.9999);
}

#[test]
fn coplanar_layers_keep_stack_order_under_a_moving_camera_with_motion_blur() {
    // Hand balance collage: a motion-blurred background under unblurred layers in the same
    // plane hid them while the camera dollied (its sub-sample depths sorted it in front).
    let (mut p, cid, comp) = setup();
    // (Comp motion blur is off by default.)
    p.comp_mut(cid).unwrap().enable_motion_blur = true;
    let mut cam = camera(&mut p, &comp);
    let pr = cam.props.prop_mut("transform/position").unwrap();
    pr.keys = vec![
        effectcraft_keyframe::Keyframe::new(Tick::ZERO, Value::Vec3([100.0, 50.0, -zoom()])),
        effectcraft_keyframe::Keyframe::new(Tick::from_seconds_f64(1.0), Value::Vec3([100.0, 50.0, -zoom() + 600.0])),
    ];
    let top = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 40, 40, [100.0, 50.0, 0.0]);
    let mut bottom = solid3(&mut p, &comp, [0.0, 0.0, 1.0], 400, 200, [100.0, 50.0, 0.0]);
    bottom.switches.motion_blur = true;
    push(&mut p, cid, cam);
    push(&mut p, cid, top);
    push(&mut p, cid, bottom);
    for t in [0.2, 0.5] {
        let img = render_frame(&p, cid, Tick::from_seconds_f64(t), 1.0);
        let c = img.get(100, 50);
        assert!(c[0] > 0.99 && c[2] < 0.01, "t={t}: {c:?}");
    }
}

#[test]
fn coplanar_layers_keep_stack_order_on_a_turned_plane_under_a_panning_camera() {
    // L3-hours-minutes (3 Nov 2026): a clock text over its motion-blurred pill body, both on
    // one plane, vanished on some frames while the camera moved. A blurred plane's sub-samples
    // look through the sample's camera, so on a plane turned to the view they hit another
    // point than the frame camera's ray does, and its depth sorted it in front of (or behind)
    // the unblurred layer on the same plane.
    for to in [200.0, 0.0] {
        let (mut p, cid, comp) = setup();
        p.comp_mut(cid).unwrap().enable_motion_blur = true;
        let mut cam = camera(&mut p, &comp);
        cam.auto_orient = effectcraft_project::AutoOrient::Off;
        let pr = cam.props.prop_mut("transform/position").unwrap();
        // Through x = 100 (looking at the layers) at t = 0.5, panning 200 px/s either way.
        pr.keys = vec![
            effectcraft_keyframe::Keyframe::new(Tick::ZERO, Value::Vec3([200.0 - to, 50.0, -zoom()])),
            effectcraft_keyframe::Keyframe::new(Tick::from_seconds_f64(1.0), Value::Vec3([to, 50.0, -zoom()])),
        ];
        let mut top = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 40, 40, [100.0, 50.0, 0.0]);
        let mut bottom = solid3(&mut p, &comp, [0.0, 0.0, 1.0], 160, 160, [100.0, 50.0, 0.0]);
        for l in [&mut top, &mut bottom] {
            set(l, "transform/rotationY", Value::Scalar(40.0));
        }
        bottom.switches.motion_blur = true;
        push(&mut p, cid, cam);
        push(&mut p, cid, top);
        push(&mut p, cid, bottom);
        let img = render_frame(&p, cid, Tick::from_seconds_f64(0.5), 1.0);
        for x in [94, 100, 106] {
            let c = img.get(x, 50);
            assert!(c[0] > 0.99 && c[2] < 0.01, "pan to {to}, x={x}: {c:?}");
        }
    }
}

/// A one-node camera layer 60 px right of the default camera, its In point at `in_frame`, in a
/// comp with motion blur at shutter phase −90° (samples from a quarter frame before the frame),
/// and a still, motion-blurred 3D solid at the comp centre.
fn camera_from_frame(in_frame: i64) -> (Project, ItemId) {
    let (mut p, cid, comp) = setup();
    {
        let c = p.comp_mut(cid).unwrap();
        c.enable_motion_blur = true;
        c.shutter_angle = 180.0;
        c.shutter_phase = -90.0;
    }
    let mut cam = camera(&mut p, &comp);
    cam.auto_orient = effectcraft_project::AutoOrient::Off;
    set(&mut cam, "transform/position", Value::Vec3([160.0, 50.0, -zoom()]));
    let start = Tick(comp.frame_rate.frame_duration().0 * in_frame);
    cam.start_time = start;
    cam.in_point = start;
    let mut s = solid3(&mut p, &comp, [1.0, 0.0, 0.0], 20, 20, [100.0, 50.0, 0.0]);
    s.switches.motion_blur = true;
    push(&mut p, cid, cam);
    push(&mut p, cid, s);
    (p, cid)
}

#[test]
fn shutter_samples_before_the_camera_in_point_keep_the_frame_camera() {
    // AE 2026 (2 Nov 2026): on a camera layer's first frame with shutter phase −90°, the samples
    // before its In point (before frame 0, or before a later In point) still look through it.
    // EffectCraft switched those samples to the default camera and drew a ghost copy of every
    // layer where the default camera sees it.
    for in_frame in [0, 10] {
        let (p, cid) = camera_from_frame(in_frame);
        let t = Tick(p.comp(cid).unwrap().frame_rate.frame_duration().0 * in_frame);
        let img = render_frame(&p, cid, t, 1.0);
        // Through the camera layer (60 px right) the solid sits at x = 40, fully opaque.
        let seen = img.get(40, 50);
        assert!(seen[0] > 0.99 && seen[3] > 0.99, "in frame {in_frame}: through the camera {seen:?}");
        // No ghost where the default camera would show it.
        let ghost = img.get(100, 50);
        assert!(ghost[3] < 0.01, "in frame {in_frame}: ghost {ghost:?}");
    }
    // The frame before a later In point still looks through the default camera.
    let (p, cid) = camera_from_frame(10);
    let img = render_frame(&p, cid, Tick(p.comp(cid).unwrap().frame_rate.frame_duration().0 * 9), 1.0);
    assert!(img.get(100, 50)[3] > 0.99 && img.get(40, 50)[3] < 0.01);
}

// ---------------------------------------------------------------- minification

/// A 3D precomp of thin white stripes (3 px every 7 px) shrunk to 10 %, beside the same layer in
/// 2D: the 3D plane must be box-filtered like the 2D warp, not point-sampled (a small render of a
/// shrunk logo showed fat bars or nothing at all: the contact-sheet "dropped layer", 2 Nov 2026).
fn shrunk_stripes(three_d: bool) -> effectcraft_raster::Image {
    let (mut p, cid, comp) = setup();
    let inner = Comp::new(140, 140, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let iid = p.add_item("Stripes", Label::Sandstone, None, ItemKind::Comp(inner.clone().into()));
    for x in (0..140).step_by(7) {
        let sid = p.add_item("Stripe", Label::Red, None, ItemKind::Solid(Solid { color: [1.0; 3], width: 3, height: 140, pixel_aspect: 1.0 }));
        let mut s = build::layer(&mut p, &inner, "Stripe", LayerSource::Solid { item: sid }, (3, 140), None);
        set(&mut s, "transform/position", Value::Vec3([x as f64 + 1.5, 70.0, 0.0]));
        p.comp_mut(iid).unwrap().layers.push(s);
    }
    let mut pre = build::layer(&mut p, &comp, "Stripes", LayerSource::Comp { item: iid }, (140, 140), None);
    pre.switches.three_d = three_d;
    set(&mut pre, "transform/scale", Value::Vec3([10.0, 10.0, 100.0]));
    push(&mut p, cid, pre);
    render(&p, cid)
}

#[test]
fn shrunk_plane_is_filtered_like_2d() {
    let (flat, plane) = (shrunk_stripes(false), shrunk_stripes(true));
    // The layer covers x 93..107, y 43..57; its inside is 3/7 white on average.
    for y in 45..55 {
        for x in 95..105 {
            let (a, b) = (flat.get(x, y)[3], plane.get(x, y)[3]);
            assert!((a - b).abs() < 0.08, "({x}, {y}): 2D {a}, 3D {b}");
            assert!((b - 3.0 / 7.0).abs() < 0.2, "({x}, {y}): 3D {b}");
        }
    }
}

#[test]
fn minify_levels_follow_the_least_shrunk_point() {
    let (mut p, cid, comp) = setup();
    let l = solid3(&mut p, &comp, [1.0; 3], 40, 20, [100.0, 50.0, 0.0]);
    push(&mut p, cid, l);
    let r = Renderer::new(&p, &NoFootage, RenderOpts::default());
    let c = p.comp(cid).unwrap();
    let ctx = crate::EvalCtx::new(&p, cid, c, Tick::ZERO);
    let layer = c.layers.first().unwrap();
    let cam = compose::camera_for(&r, &ctx);
    let world = ctx.world_matrix(layer);
    let g = compose::geo(&cam, world, [0.0, 0.0, 40.0, 20.0], (W as f64, H as f64), 1.0, (W, H), (0.0, 0.0)).unwrap();
    // At z = 0 one layer unit is one output pixel: 1 buffer px per unit needs no level, 10 need
    // three (10 → 5 → 2.5 → 1.25), and a 1×1 buffer can't be halved.
    assert_eq!(compose::minify_levels(std::slice::from_ref(&g), 1.0, (40, 20)), 0);
    assert_eq!(compose::minify_levels(std::slice::from_ref(&g), 10.0, (400, 200)), 3);
    assert_eq!(compose::minify_levels(std::slice::from_ref(&g), 10.0, (1, 1)), 0);
}
