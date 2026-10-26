//! A feathered mask that runs past the layer's edge is blurred as drawn, not as cut off there:
//! along that edge the mask is still fully on (an inverted one fully off). The feather used to
//! treat the outside of the layer as uncovered, which left a faint band along the edge of an
//! inverted mask (a dark gradient solid whose inverted mask hangs below its bottom edge).

use effectcraft_color::Label;
use effectcraft_keyframe::{ShapePath, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Comp, GroupKind, ItemId, ItemKind, LayerSource, MaskMode, Node, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{NoFootage, RenderOpts, Renderer};

/// A white 100×100 solid with a mask from y 60 down to y 160 (past the bottom edge at 100),
/// feathered 20 px.
fn project(inverted: bool, linear: bool) -> (Project, ItemId) {
    let mut p = Project::default();
    let comp = Comp::new(100, 100, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: [1.0; 3], width: 100, height: 100, pixel_aspect: 1.0 }));
    let mut l = build::layer(&mut p, &comp, "S", LayerSource::Solid { item: sid }, (100, 100), None);
    let mut next = p.next_id;
    let mut m = build::mask(&mut Ids(&mut next), "Mask 1", ShapePath::rect([50.0, 110.0], 160.0, 100.0), MaskMode::Add, [255, 0, 0]);
    p.next_id = next;
    if let GroupKind::Mask { inverted: inv, feather_falloff, .. } = &mut m.kind {
        *inv = inverted;
        if linear {
            *feather_falloff = effectcraft_project::FeatherFalloff::Linear;
        }
    }
    m.get_mut("feather").unwrap().value = Value::Vec2([20.0, 20.0]);
    l.props.sub_mut("masks").unwrap().children.push(Node::Group(m));
    let cid = p.add_item("C", Label::Sandstone, None, ItemKind::Comp(comp.into()));
    p.comp_mut(cid).unwrap().layers.push(l);
    (p, cid)
}

fn alpha(p: &Project, cid: ItemId, x: i64, y: i64) -> f32 {
    Renderer::new(p, &NoFootage, RenderOpts::default()).comp_frame(cid, Tick::ZERO).get(x, y)[3]
}

#[test]
fn a_feathered_mask_past_the_layer_edge_stays_covered_at_that_edge() {
    for linear in [false, true] {
        let (p, cid) = project(false, linear);
        let a = alpha(&p, cid, 50, 99);
        assert!(a > 0.99, "bottom row inside the mask (linear {linear}): {a}");
        let a = alpha(&p, cid, 50, 30);
        assert!(a < 0.01, "above the feathered top edge (linear {linear}): {a}");
        let (p, cid) = project(true, linear);
        let a = alpha(&p, cid, 50, 99);
        assert!(a < 0.01, "inverted: no band along the bottom edge (linear {linear}): {a}");
        let a = alpha(&p, cid, 50, 30);
        assert!(a > 0.99, "inverted: on above the mask (linear {linear}): {a}");
    }
}
