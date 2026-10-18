//! Frame fingerprints ([`Renderer::frame_fingerprint`]): an edit keeps the fingerprint exactly
//! of the frames it doesn't change (checked against fresh CPU renders), and gives up where it
//! can't tell.

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{BitDepth, Comp, Expression, ItemId, ItemKind, Layer, LayerSource, Node, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{EvalCtx, ExprHost, Image, NoFootage, RenderOpts, Renderer};

fn s(x: f64) -> Tick {
    Tick::from_seconds_f64(x)
}

fn project() -> Project {
    let mut p = Project::default();
    p.settings.bit_depth = BitDepth::Bpc32;
    p
}

fn comp(p: &mut Project, name: &str) -> (ItemId, Comp) {
    let c = Comp::new(100, 80, FrameRate::FPS_30, s(4.0));
    (p.add_item(name, Label::Sandstone, None, ItemKind::Comp(c.clone().into())), c)
}

fn solid(p: &mut Project, c: &Comp, color: [f32; 3]) -> Layer {
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color, width: 40, height: 30, pixel_aspect: 1.0 }));
    build::layer(p, c, "S", LayerSource::Solid { item: sid }, (40, 30), None)
}

fn effect(p: &mut Project, l: &mut Layer, id: &str) {
    let spec = effectcraft_effects::find(id).unwrap();
    let mut next = p.next_id;
    let g = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), spec.name, [40.0, 30.0]);
    p.next_id = next;
    l.props.sub_mut("effects").unwrap().children.push(g.into());
}

fn keys(times: &[f64], value: impl Fn(usize) -> Value) -> Vec<Keyframe> {
    times.iter().enumerate().map(|(i, t)| Keyframe::new(s(*t), value(i))).collect()
}

/// Position keyframes at 0, 1, 2 and 3 s.
fn moving(l: &mut Layer) {
    l.props.prop_mut("transform/position").unwrap().keys = keys(&[0.0, 1.0, 2.0, 3.0], |i| Value::Vec3([20.0 + 15.0 * i as f64, 30.0 + 5.0 * i as f64, 0.0]));
}

/// A comp with a moving solid over a static one.
fn scene() -> (Project, ItemId) {
    let mut p = project();
    let (cid, c) = comp(&mut p, "Main");
    let mut top = solid(&mut p, &c, [1.0, 0.4, 0.1]);
    moving(&mut top);
    let bg = solid(&mut p, &c, [0.1, 0.2, 0.9]);
    p.comp_mut(cid).unwrap().layers = vec![top, bg].into();
    (p, cid)
}

fn renderer<'a>(p: &'a Project, expr: Option<&'a dyn ExprHost>) -> Renderer<'a> {
    let mut r = Renderer::new(p, &NoFootage, RenderOpts::default());
    r.expr = expr;
    r
}

fn print(p: &Project, cid: ItemId, t: Tick) -> Option<u64> {
    renderer(p, None).frame_fingerprint(cid, t)
}

/// Every 5th frame of the 4 s comp.
/// The comp's end (4 s): no layer is on screen there, so no edit to a layer changes it.
const END: i64 = 120;

fn frames() -> impl Iterator<Item = i64> {
    (0..=120).step_by(5)
}

/// The frames whose fingerprint `before` and `after` share. For each, the pixels must be equal
/// (the CPU oracle: a retained frame equals a fresh render).
fn kept(before: &Project, after: &Project, cid: ItemId) -> Vec<i64> {
    kept_with(before, after, cid, None)
}

/// [`kept`] with an expression engine.
fn kept_with(before: &Project, after: &Project, cid: ItemId, expr: Option<&dyn ExprHost>) -> Vec<i64> {
    let fps = before.comp(cid).unwrap().frame_rate;
    let mut kept = vec![];
    for f in frames() {
        let t = fps.tick_of(f);
        let (a, b) = (renderer(before, expr).frame_fingerprint(cid, t), renderer(after, expr).frame_fingerprint(cid, t));
        if a.is_some() && a == b {
            let (x, y): (Image, Image) = (renderer(before, expr).comp_frame(cid, t), renderer(after, expr).comp_frame(cid, t));
            assert_eq!((x.width, x.height), (y.width, y.height));
            assert!(x.data == y.data, "frame {f}: same fingerprint, different pixels");
            kept.push(f);
        }
    }
    kept
}

fn edit(p: &Project, cid: ItemId, f: impl FnOnce(&mut Layer)) -> Project {
    let mut q = p.clone();
    f(q.comp_mut(cid).unwrap().layers.get_mut(0).unwrap());
    q
}

#[test]
fn a_keyframe_edit_keeps_the_frames_outside_its_neighbours() {
    let (p, cid) = scene();
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[1].value = Value::Vec3([70.0, 10.0, 0.0]));
    // Key 1 (1 s) moved: frames between 0 and 2 s changed; 0 s, 2 s and from the key after
    // next on (3 s: the auto-Bézier motion path bends key 2's tangent) are unchanged.
    let kept = kept(&p, &q, cid);
    assert!([0, 60].into_iter().chain((90..=120).step_by(5)).all(|f| kept.contains(&f)), "{kept:?}");
    assert!((5..60).step_by(5).all(|f| !kept.contains(&f)), "{kept:?}");
}

#[test]
fn a_keyframe_edit_in_the_first_two_seconds_keeps_the_frames_after_the_next_keyframe() {
    let (p, cid) = scene();
    // Keyframes at 0 and 2 s only; the 0 s one changes.
    let p = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys.retain(|k| k.time == Tick::ZERO || k.time == s(2.0)));
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[0].value = Value::Vec3([5.0, 5.0, 0.0]));
    let kept = kept(&p, &q, cid);
    assert_eq!(kept, (60..=120).step_by(5).collect::<Vec<_>>());
}

#[test]
fn an_effect_keyframe_edit_keeps_the_frames_outside_its_span() {
    let (mut p, cid) = scene();
    let mut l = p.comp(cid).unwrap().layers.get(0).unwrap().clone();
    effect(&mut p, &mut l, "ec.blur.gaussian");
    l.props.prop_mut("effects/#1/blurriness").unwrap().keys = keys(&[0.0, 2.0, 4.0], |i| Value::Scalar(2.0 + 4.0 * i as f64));
    *p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap() = l;
    let q = edit(&p, cid, |l| l.props.prop_mut("effects/#1/blurriness").unwrap().keys[2].value = Value::Scalar(1.0));
    let kept = kept(&p, &q, cid);
    assert_eq!(kept, (0..=60).step_by(5).chain([END]).collect::<Vec<_>>());
}

#[test]
fn a_static_value_edit_changes_every_frame() {
    let (p, cid) = scene();
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(50.0));
    assert_eq!(kept(&p, &q, cid), vec![END]);
    // A static effect value too.
    let (mut p, cid) = scene();
    let mut l = p.comp(cid).unwrap().layers.get(0).unwrap().clone();
    effect(&mut p, &mut l, "ec.blur.gaussian");
    *p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap() = l;
    let q = edit(&p, cid, |l| l.props.prop_mut("effects/#1/blurriness").unwrap().value = Value::Scalar(9.0));
    assert_eq!(kept(&p, &q, cid), vec![END]);
}

/// A layer on screen from 2 to 3 s: a static edit to it changes exactly the frames it shows
/// (the layers off screen at a frame count by their place alone), each kept frame equal to a
/// fresh render.
#[test]
fn a_static_edit_to_a_layer_changes_only_the_frames_it_is_on_screen() {
    let (p, cid) = scene();
    let p = edit(&p, cid, |l| {
        l.in_point = s(2.0);
        l.out_point = s(3.0);
    });
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(50.0));
    let outside: Vec<i64> = frames().filter(|f| !(60..90).contains(f)).collect();
    assert_eq!(kept(&p, &q, cid), outside);
    // Its keyframes too: a moved keyframe changes nothing outside the layer's span.
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[3].value = Value::Vec3([5.0, 5.0, 0.0]));
    let kept = kept(&p, &q, cid);
    assert!(outside.iter().all(|f| kept.contains(f)), "{kept:?}");
}

/// A parent moves its children outside its own span: an edit to a parent off screen changes
/// every frame its child shows.
#[test]
fn an_edit_to_a_parent_off_screen_changes_its_childs_frames() {
    let (mut p, cid) = scene();
    {
        let layers = &mut p.comp_mut(cid).unwrap().layers;
        let bg = layers.get(1).unwrap().id;
        let parent = layers.get_mut(1).unwrap();
        parent.in_point = s(3.5);
        parent.out_point = s(4.0);
        layers.get_mut(0).unwrap().parent = Some(bg);
    }
    let mut q = p.clone();
    q.comp_mut(cid).unwrap().layers.get_mut(1).unwrap().props.prop_mut("transform/position").unwrap().value = Value::Vec3([10.0, 10.0, 0.0]);
    assert_eq!(kept(&p, &q, cid), vec![END]);
}

/// An outer comp showing a precomp whose solid fades with keyframes at 0, 1, 2, 3 s.
fn precomp_scene() -> (Project, ItemId, ItemId) {
    let mut p = project();
    let (iid, inner) = comp(&mut p, "Inner");
    let mut l = solid(&mut p, &inner, [1.0, 0.5, 0.0]);
    l.props.prop_mut("transform/opacity").unwrap().keys = keys(&[0.0, 1.0, 2.0, 3.0], |i| Value::Scalar(100.0 - 20.0 * i as f64));
    p.comp_mut(iid).unwrap().layers.push(l);
    let (oid, outer) = comp(&mut p, "Outer");
    let pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (100, 80), None);
    let bg = solid(&mut p, &outer, [0.0, 0.3, 0.3]);
    p.comp_mut(oid).unwrap().layers = vec![pre, bg].into();
    (p, oid, iid)
}

#[test]
fn a_keyframe_edit_in_a_precomp_keeps_the_outer_frames_it_doesnt_change() {
    let (p, oid, iid) = precomp_scene();
    let q = edit(&p, iid, |l| l.props.prop_mut("transform/opacity").unwrap().keys[3].value = Value::Scalar(5.0));
    // Key 3 (3 s, the last): frames up to 2 s keep theirs (and 4 s, past the precomp's end).
    let kept = kept(&p, &q, oid);
    assert_eq!(kept, (0..=60).step_by(5).chain([END]).collect::<Vec<_>>());
}

#[test]
fn a_motion_blurred_precomp_keeps_its_nested_keyframes_out_unless_collapsed() {
    let (mut p, oid, iid) = precomp_scene();
    p.comp_mut(oid).unwrap().enable_motion_blur = true;
    let p = edit(&p, oid, |l| {
        l.switches.motion_blur = true;
        moving(l);
    });
    let q = edit(&p, iid, |l| l.props.prop_mut("transform/opacity").unwrap().keys[3].value = Value::Scalar(5.0));
    // Drawn once and its buffer moved within the shutter: the nested values at the frame decide.
    let kept = kept(&p, &q, oid);
    assert!((0..=55).step_by(5).all(|f| kept.contains(&f)), "{kept:?}");
    // Collapsed, its layers are drawn at the shutter's times: every nested keyframe counts.
    let collapse = |p: &Project| edit(p, oid, |l| l.switches.collapse = true);
    let kept = self::kept(&collapse(&p), &collapse(&q), oid);
    assert!(kept.iter().all(|f| *f == END), "{kept:?}");
}

#[test]
fn undo_returns_the_same_fingerprints() {
    let (p, cid) = scene();
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[2].value = Value::Vec3([1.0, 2.0, 0.0]));
    // Undo: an equal project in new allocations (a revert by value).
    let undone = edit(&q, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[2].value = Value::Vec3([50.0, 40.0, 0.0]));
    assert_eq!(kept(&p, &undone, cid), frames().collect::<Vec<_>>());
}

#[test]
fn time_remapping_keeps_its_keyframes_in_the_key() {
    let (p, oid, _) = precomp_scene();
    let mut next = p.next_id;
    let mut tr = Ids(&mut next).prop("timeRemap", "Time Remap", Value::Scalar(0.0));
    tr.keys = keys(&[0.0, 2.0, 4.0], |i| Value::Scalar(i as f64));
    let mut p = edit(&p, oid, |l| l.props.children.push(Node::Prop(tr)));
    p.next_id = next;
    let q = edit(&p, oid, |l| {
        l.props.get_mut("timeRemap").unwrap().keys[2].value = Value::Scalar(3.5);
    });
    // Can't prove: every frame is invalidated, even those before the edited span.
    assert_eq!(kept(&p, &q, oid), vec![END]);
}

/// Position keyframes every half second.
fn dense(l: &mut Layer) {
    let times: Vec<f64> = (0..=8).map(|i| i as f64 * 0.5).collect();
    l.props.prop_mut("transform/position").unwrap().keys = keys(&times, |i| Value::Vec3([20.0 + 7.0 * i as f64, 30.0 + 3.0 * (i % 3) as f64, 0.0]));
}

#[test]
fn motion_blur_keeps_the_keyframes_its_shutter_reads_in_the_key() {
    let (mut p, cid) = scene();
    p.comp_mut(cid).unwrap().enable_motion_blur = true;
    let p = edit(&p, cid, |l| {
        l.switches.motion_blur = true;
        dense(l);
    });
    // The last key (4 s) changes: the shutter around frames up to ~2.4 s reads none of the
    // keyframes its interpolation depends on (3 s and later).
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[8].value = Value::Vec3([0.0, 0.0, 0.0]));
    let kept = kept(&p, &q, cid);
    assert!((0..=60).step_by(5).all(|f| kept.contains(&f)), "{kept:?}");
    assert!(kept.iter().all(|f| *f < 90 || *f == END), "{kept:?}");
    // The first key (0 s): frames from ~1.6 s on stay.
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[0].value = Value::Vec3([0.0, 0.0, 0.0]));
    let kept = self::kept(&p, &q, cid);
    assert!((60..=120).step_by(5).all(|f| kept.contains(&f)), "{kept:?}");
    assert!(kept.iter().all(|f| *f > 30), "{kept:?}");
    // Keys up to 2 s, the middle one (1 s) changes: past the last key (held) frames stay.
    let p = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys.truncate(5));
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[2].value = Value::Vec3([0.0, 0.0, 0.0]));
    let kept = self::kept(&p, &q, cid);
    assert!((65..=120).step_by(5).all(|f| kept.contains(&f)), "{kept:?}");
    assert!(!kept.contains(&30), "{kept:?}");
}

#[test]
fn a_motion_blurred_layer_reading_other_layers_where_its_shutter_reads_has_no_fingerprint() {
    let (mut p, cid) = scene();
    p.comp_mut(cid).unwrap().enable_motion_blur = true;
    let reads = |path: &'static str| {
        edit(&p, cid, move |l| {
            l.switches.motion_blur = true;
            l.props.prop_mut(path).unwrap().expr = Some(Expression { text: "thisComp.layer(2).transform.opacity".into(), enabled: true });
        })
    };
    // Opacity applies once, at the frame time: its value there is in the key.
    assert!(print(&reads("transform/opacity"), cid, s(1.0)).is_some());
    // Rotation is read at each sub-sample.
    assert_eq!(print(&reads("transform/rotation"), cid, s(1.0)), None);
}

#[test]
fn a_motion_blurred_layers_parent_keeps_the_keyframes_its_shutter_reads_in_the_key() {
    let (mut p, cid) = scene();
    p.comp_mut(cid).unwrap().enable_motion_blur = true;
    // The static background solid becomes the moving one's parent, moving itself (dense keys).
    let parent = p.comp(cid).unwrap().layers.get(1).unwrap().id;
    let mut p = edit(&p, cid, |l| {
        l.switches.motion_blur = true;
        l.parent = Some(parent);
    });
    dense(p.comp_mut(cid).unwrap().layers.get_mut(1).unwrap());
    let mut q = p.clone();
    q.comp_mut(cid).unwrap().layers.get_mut(1).unwrap().props.prop_mut("transform/position").unwrap().keys[8].value = Value::Vec3([0.0, 0.0, 0.0]);
    let kept = kept(&p, &q, cid);
    assert!((0..=60).step_by(5).all(|f| kept.contains(&f)), "{kept:?}");
    assert!(kept.iter().all(|f| *f < 90 || *f == END), "{kept:?}");
}

#[test]
fn a_time_reader_meeting_an_expression_has_no_fingerprint() {
    let (mut p, cid) = scene();
    let mut l = p.comp(cid).unwrap().layers.get(0).unwrap().clone();
    effect(&mut p, &mut l, "ec.time.echo");
    effect(&mut p, &mut l, "ec.blur.gaussian");
    let other = Some(Expression { text: "thisComp.layer(2).transform.opacity * 0.5".into(), enabled: true });
    l.props.prop_mut("effects/#2/blurriness").unwrap().expr = other.clone();
    *p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap() = l;
    // The Echo draws the layer's effects at other times: the expression may read anything there.
    assert_eq!(print(&p, cid, s(1.0)), None);
    // Its transform applies at the frame time alone, where its values are in the key.
    let p = edit(&p, cid, |l| {
        l.props.prop_mut("effects/#2/blurriness").unwrap().expr = None;
        l.props.prop_mut("transform/opacity").unwrap().expr = other.clone();
    });
    assert!(print(&p, cid, s(1.0)).is_some());
    // An expression of its own value and the time: its keyframes (and the time) stay in the key.
    let p = edit(&p, cid, |l| {
        l.props.prop_mut("transform/opacity").unwrap().expr = Some(Expression { text: "Math.min(value, 50 + time * 10) // thisComp".into(), enabled: true })
    });
    assert!(print(&p, cid, s(1.0)).is_some());
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[3].value = Value::Vec3([0.0, 0.0, 0.0]));
    assert_eq!(kept(&p, &q, cid), vec![END]);
}

#[test]
fn a_time_reader_keeps_the_other_layers_free() {
    let (mut p, cid) = scene();
    let mut bg = p.comp(cid).unwrap().layers.get(1).unwrap().clone();
    effect(&mut p, &mut bg, "ec.time.echo");
    *p.comp_mut(cid).unwrap().layers.get_mut(1).unwrap() = bg;
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/position").unwrap().keys[1].value = Value::Vec3([70.0, 10.0, 0.0]));
    let kept = self::kept(&p, &q, cid);
    assert!([0, 60].into_iter().chain((90..=120).step_by(5)).all(|f| kept.contains(&f)), "{kept:?}");
    // As an adjustment layer on top, the Echo reads the layers below at the frame time alone.
    let adjust = |p: &Project| {
        let mut p = p.clone();
        let c = p.comp_mut(cid).unwrap();
        let mut bg = c.layers.get(1).unwrap().clone();
        bg.switches.adjustment = true;
        let top = c.layers.get(0).unwrap().clone();
        c.layers = vec![bg, top].into();
        p
    };
    let (p, q) = (adjust(&p), adjust(&q));
    let kept = self::kept(&p, &q, cid);
    assert!([0, 60].into_iter().chain((90..=120).step_by(5)).all(|f| kept.contains(&f)), "{kept:?}");
}

#[test]
fn self_contained_expressions() {
    use crate::cache::free::self_contained;
    for e in ["value + [0, 24*(time-3.07)]", "Math.floor(time*29.97)*7", "var x = time * 2; x + value", "'thisComp' + value", "/* thisLayer */ value", "0.5"] {
        assert!(self_contained(e), "{e}");
    }
    for e in [
        "thisComp.layer(1).position",
        "valueAtTime(time - 1)",
        "effect(\"Slider\")(1)",
        "loopOut()",
        "comp(\"A\").time",
        "`${thisComp}`",
        "x",
        "index * 2",
        "transform.position",
    ] {
        assert!(!self_contained(e), "{e}");
    }
}

// ------------------------------------------------------------------------------ layer cache keys

/// The layer cache key of the first layer at `t` (as `Renderer::content_cache_key` asks it).
fn layer_key_at(p: &Project, cid: ItemId, t: Tick) -> Option<u64> {
    let c = p.comp(cid).unwrap();
    let l = c.layers.get(0).unwrap();
    crate::cache::layer_key(&EvalCtx::new(p, cid, c, t), l, 1.0, false, false)
}

/// A keyframe edit on an effect keeps the layer's cached pixels at the frames whose values it
/// doesn't change (its key there is unchanged), and those pixels equal a fresh render.
#[test]
fn an_effect_keyframe_edit_keeps_the_layer_keys_outside_its_span() {
    let (mut p, cid) = scene();
    let mut l = p.comp(cid).unwrap().layers.get(0).unwrap().clone();
    effect(&mut p, &mut l, "ec.blur.gaussian");
    l.props.prop_mut("effects/#1/blurriness").unwrap().keys = keys(&[0.0, 2.0, 4.0], |i| Value::Scalar(2.0 + 4.0 * i as f64));
    *p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap() = l;
    let q = edit(&p, cid, |l| l.props.prop_mut("effects/#1/blurriness").unwrap().keys[2].value = Value::Scalar(1.0));
    let fps = p.comp(cid).unwrap().frame_rate;
    let kept: Vec<i64> =
        frames().filter(|f| layer_key_at(&p, cid, fps.tick_of(*f)).is_some_and(|k| Some(k) == layer_key_at(&q, cid, fps.tick_of(*f)))).collect();
    assert_eq!(kept, (0..=60).step_by(5).collect::<Vec<_>>());
    // Through the cache: frames rendered before the edit, then after it from the same cache,
    // equal fresh renders.
    let cache = crate::LayerCache::default();
    for f in frames() {
        let mut r = renderer(&p, None);
        r.cache = Some(&cache);
        r.comp_frame(cid, fps.tick_of(f));
    }
    for f in frames() {
        let mut r = renderer(&q, None);
        r.cache = Some(&cache);
        let cached = r.comp_frame(cid, fps.tick_of(f));
        assert!(cached.data == renderer(&q, None).comp_frame(cid, fps.tick_of(f)).data, "frame {f}");
    }
}

/// Motion blur keeps the keyframes in a layer's key (its content is drawn at the shutter's
/// times): every key changes.
#[test]
fn a_motion_blurred_layer_keeps_its_keyframes_in_its_key() {
    let (mut p, cid) = scene();
    let mut l = p.comp(cid).unwrap().layers.get(0).unwrap().clone();
    effect(&mut p, &mut l, "ec.blur.gaussian");
    l.props.prop_mut("effects/#1/blurriness").unwrap().keys = keys(&[0.0, 2.0, 4.0], |i| Value::Scalar(2.0 + 4.0 * i as f64));
    *p.comp_mut(cid).unwrap().layers.get_mut(0).unwrap() = l;
    let q = edit(&p, cid, |l| l.props.prop_mut("effects/#1/blurriness").unwrap().keys[2].value = Value::Scalar(1.0));
    let c = p.comp(cid).unwrap();
    let key = |p: &Project, t: Tick| {
        let c = p.comp(cid).unwrap();
        crate::cache::layer_key(&EvalCtx::new(p, cid, c, t), c.layers.get(0).unwrap(), 1.0, false, true)
    };
    let t = c.frame_rate.tick_of(0);
    assert_ne!(key(&p, t), key(&q, t), "blurred: the keyframes count");
}

/// Measure on a real project (`EC_PRINTS_PROJECT=…/project.ecproj`): for each layer of every
/// comp with a keyframed content property (masks, effects, text, shapes), edit its middle
/// keyframe and count the frames (up to 600 spread over the comp) whose layer cache key stays
/// (before this change: none, as the key hashed the keyframes).
#[test]
#[ignore = "needs a local project file; prints counts"]
fn real_project_layer_keys_kept_after_one_keyframe_edit() {
    use effectcraft_project::PropGroup;
    let Ok(path) = std::env::var("EC_PRINTS_PROJECT") else { return };
    let p = Project::from_json(&std::fs::read_to_string(path).unwrap()).unwrap();
    fn first_keyed(g: &mut PropGroup) -> Option<&mut effectcraft_project::Property> {
        for c in &mut g.children {
            match c {
                Node::Prop(p) if p.keys.len() >= 3 && matches!(p.keys[p.keys.len() / 2].value, Value::Scalar(_) | Value::Vec2(_) | Value::Vec3(_)) => {
                    return Some(p);
                }
                Node::Prop(_) => {}
                Node::Group(sub) if sub.match_id != "transform" => {
                    if let Some(p) = first_keyed(sub) {
                        return Some(p);
                    }
                }
                Node::Group(_) => {}
            }
        }
        None
    }
    let (mut edits, mut kept_total, mut keyed_total) = (0usize, 0usize, 0usize);
    let comps: Vec<(ItemId, Comp)> =
        p.items.iter().filter_map(|(id, i)| if let ItemKind::Comp(c) = &i.kind { Some((*id, Comp::clone(c))) } else { None }).collect();
    for (cid, comp) in comps {
        let fps = comp.frame_rate;
        let total = fps.frame_at(comp.duration).max(1);
        let n = total.min(600);
        let times: Vec<Tick> = (0..n).map(|i| fps.tick_of(i * total / n)).collect();
        for i in 0..comp.layers.len() {
            let mut q = p.clone();
            let c = q.comp_mut(cid).unwrap();
            let Some(prop) = first_keyed(&mut c.layers.get_mut(i).unwrap().props) else { continue };
            let k = prop.keys.len() / 2;
            prop.keys[k].value = match &prop.keys[k].value {
                Value::Scalar(v) => Value::Scalar(v * 0.5 + 1.0),
                Value::Vec2(v) => Value::Vec2([v[0] + 7.0, v[1] - 3.0]),
                Value::Vec3(v) => Value::Vec3([v[0] + 7.0, v[1] - 3.0, v[2]]),
                v => v.clone(),
            };
            let key = |p: &Project, t: Tick| {
                let c = p.comp(cid).unwrap();
                crate::cache::layer_key(&EvalCtx::new(p, cid, c, t), c.layers.get(i).unwrap(), 1.0, false, false)
            };
            let before: Vec<Option<u64>> = times.iter().map(|t| key(&p, *t)).collect();
            let keyed = before.iter().filter(|k| k.is_some()).count();
            if keyed == 0 {
                continue;
            }
            let kept = times.iter().zip(&before).filter(|(t, b)| b.is_some() && key(&q, **t) == **b).count();
            eprintln!("{:?} layer {i} {:?}: key kept at {kept}/{keyed} keyed frames", p.item(cid).unwrap().name, comp.layers.get(i).unwrap().name);
            edits += 1;
            kept_total += kept;
            keyed_total += keyed;
        }
    }
    eprintln!("{edits} content keyframe edits: {:.1} % of layer keys kept ({kept_total}/{keyed_total})", 100.0 * kept_total as f64 / keyed_total.max(1) as f64);
}

/// An expression engine whose every expression reads the first layer's opacity a second later
/// (`thisComp.layer(1).transform.opacity.valueAtTime(time + 1)`).
struct ReadsAhead;

impl ExprHost for ReadsAhead {
    fn eval(&self, ctx: &EvalCtx, _: &Layer, _: &effectcraft_project::Property, value: &Value) -> Result<Value, String> {
        let Some(first) = ctx.comp.layers.get(0) else { return Ok(value.clone()) };
        let Some(p) = first.props.prop("transform/opacity") else { return Ok(value.clone()) };
        Ok(ctx.at(ctx.time + s(1.0)).value(first, p))
    }
}

#[test]
fn an_expression_reading_another_layer_at_another_time_follows_the_edit() {
    let (p, cid) = scene();
    let p = edit(&p, cid, |l| l.props.prop_mut("transform/opacity").unwrap().keys = keys(&[0.0, 1.0, 2.0, 3.0], |i| Value::Scalar(100.0 - 20.0 * i as f64)));
    let mut p = p;
    let bg = p.comp_mut(cid).unwrap().layers.get_mut(1).unwrap();
    bg.props.prop_mut("transform/opacity").unwrap().expr =
        Some(Expression { text: "thisComp.layer(1).transform.opacity.valueAtTime(time + 1)".into(), enabled: true });
    // The last key (3 s) changes the first layer from 2 s on, the second from 1 s on.
    let q = edit(&p, cid, |l| l.props.prop_mut("transform/opacity").unwrap().keys[3].value = Value::Scalar(0.0));
    assert_eq!(kept_with(&p, &q, cid, Some(&ReadsAhead)), (0..=30).step_by(5).chain([END]).collect::<Vec<_>>());
    // Without the expression the second layer holds still: the frames up to 2 s are kept.
    assert_eq!(kept_with(&p, &q, cid, None), (0..=60).step_by(5).chain([END]).collect::<Vec<_>>());
}

/// Why a real project's frames have no fingerprint (`EC_PRINTS_PROJECT=…`): per layer of the
/// largest comp (and the comps it nests), its expressions and what reads other times.
#[test]
#[ignore = "needs a local project file; prints a diagnosis"]
fn real_project_fingerprint_diagnosis() {
    let Ok(path) = std::env::var("EC_PRINTS_PROJECT") else { return };
    let p = Project::from_json(&std::fs::read_to_string(path).unwrap()).unwrap();
    let (cid, comp) = p
        .items
        .iter()
        .filter_map(|(id, i)| if let ItemKind::Comp(c) = &i.kind { Some((*id, c.clone())) } else { None })
        .max_by_key(|(_, c)| c.layers.len())
        .unwrap();
    let mut stack = vec![(cid, 0usize)];
    let mut seen = std::collections::HashSet::new();
    while let Some((id, depth)) = stack.pop() {
        if !seen.insert(id) || depth > 4 {
            continue;
        }
        let c = p.comp(id).unwrap();
        let key = renderer(&p, None).comp_key(id, s(1.0)).is_some();
        eprintln!("{}comp {:?} ({} layers) motion blur {} keyed {key}", "  ".repeat(depth), p.item(id).unwrap().name, c.layers.len(), c.enable_motion_blur);
        for l in c.layers.iter() {
            let i = crate::cache::layer_info(c, l);
            if !i.pointwise && (i.layer_refs || i.reads_comp_scene || i.paint) {
                eprintln!("{}  SPREAD {:?} adj {} refs {} scene {}", "  ".repeat(depth), l.name, l.switches.adjustment, i.layer_refs, i.reads_comp_scene);
            }
            let children: Vec<&str> = c.layers.iter().filter(|k| k.parent == Some(l.id)).map(|k| k.name.as_str()).collect();
            if i.expressions && !i.contained && !children.is_empty() {
                eprintln!("{}  PARENT {:?} of {:?}", "  ".repeat(depth), l.name, children);
            }
            if i.expressions && !i.contained {
                fn exprs(g: &effectcraft_project::PropGroup, out: &mut Vec<String>) {
                    for c in &g.children {
                        match c {
                            Node::Prop(p) => {
                                if let Some(e) = p.expr.as_ref().filter(|e| e.enabled && !crate::cache::free::self_contained(&e.text)) {
                                    out.push(format!("{}: {}", p.match_id, e.text.chars().take(80).collect::<String>().replace('\n', " ")));
                                }
                            }
                            Node::Group(sub) => exprs(sub, out),
                        }
                    }
                }
                let mut out = vec![];
                exprs(&l.props, &mut out);
                eprintln!("{}  {:?} mb {} pointwise {}: {:?}", "  ".repeat(depth), l.name, l.switches.motion_blur, i.pointwise, out);
            }
            if let LayerSource::Comp { item } = l.source {
                stack.push((item, depth + 1));
            }
        }
    }
    let r = renderer(&p, None);
    eprintln!("fingerprint at 1 s: {:?}", r.frame_fingerprint(cid, s(1.0)));
    let _ = comp;
}
