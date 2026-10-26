//! The Composition viewer's Selection-tool maths for frontends that show a comp somewhere else
//! (FigCraft's EffectCraft cards): which layer a comp point hits, a layer's box and its eight
//! handles in comp pixels, and the Position / Scale values a move or handle drag writes (with
//! `prop.set`, which keys animated properties at the current time). 2D layers through their
//! parents, 3D layers through the comp's active camera, as the viewer's main view does.
//!
//! Nothing here touches the project; edits go through commands.

use effectcraft_geom::{Mat3, Mat4, vec2};
use effectcraft_project::{Comp, Layer, LayerId};
use effectcraft_render::EvalCtx;
use effectcraft_time::Tick;

/// Below this homogeneous `w` a box corner is at or behind the camera (the box would flip).
const NEAR_W: f64 = 0.5;

/// The number of box handles: the corners clockwise from the top left (0–3), then the top,
/// right, bottom and left edges' midpoints (4–7).
pub const HANDLES: usize = 8;

/// A layer's content box on the comp: layer → comp matrix, the box's corners in comp pixels
/// (clockwise from the top left) and the box in layer pixels (`[x0, y0, x1, y1]`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerBox {
    pub layer: LayerId,
    pub l2c: Mat3,
    pub quad: [[f64; 2]; 4],
    pub bounds: [f64; 4],
}

impl LayerBox {
    /// Handle `handle`'s point in comp pixels (handles as in [`HANDLES`]).
    pub fn handle(&self, handle: usize) -> [f64; 2] {
        let p = handle_point(self.bounds, handle);
        let q = self.l2c.apply(vec2(p[0], p[1]));
        [q.x, q.y]
    }

    /// All eight handles in comp pixels.
    pub fn handles(&self) -> [[f64; 2]; HANDLES] {
        std::array::from_fn(|i| self.handle(i))
    }
}

/// `p` through `m` before the perspective divide: (x, y, w).
fn homogeneous(m: &Mat3, p: [f64; 2]) -> [f64; 3] {
    let r = |i: usize| m.0.get(i).map_or(0.0, |row| row[0] * p[0] + row[1] * p[1] + row[2]);
    [r(0), r(1), r(2)]
}

/// `layer`'s content box on the comp, or None (no bounds, or a 3D layer reaching to or behind
/// the camera).
pub fn layer_box(ctx: &EvalCtx, layer: &Layer) -> Option<LayerBox> {
    let b = effectcraft_render::content_bounds(ctx, layer)?;
    if b.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let (m, _) = ctx.layer_to_comp(layer);
    let corners = [[b[0], b[1]], [b[2], b[1]], [b[2], b[3]], [b[0], b[3]]];
    if corners.iter().any(|p| homogeneous(&m, *p)[2] < NEAR_W) {
        return None;
    }
    let quad = corners.map(|p| {
        let q = m.apply(vec2(p[0], p[1]));
        [q.x, q.y]
    });
    quad.iter().flatten().all(|v| v.is_finite()).then_some(LayerBox { layer: layer.id, l2c: m, quad, bounds: b })
}

/// Visible, unlocked layers a click can hit, front to back (the viewer's rule: no cameras or
/// lights; with a soloed AV layer only the soloed ones).
pub fn selectable_layers(comp: &Comp, time: Tick) -> impl Iterator<Item = &Layer> {
    let any_solo = comp.layers.iter().any(|l| l.switches.solo && l.source.is_av());
    comp.layers
        .iter()
        .filter(move |l| l.is_active_at(time) && l.switches.video && !l.switches.locked && !l.is_camera() && !l.is_light() && (!any_solo || l.switches.solo))
}

/// Whether comp point `p` is inside the convex quad `q`.
pub fn point_in_quad(p: [f64; 2], q: &[[f64; 2]; 4]) -> bool {
    let mut sign = 0.0;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let c = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
        if c.abs() < 1e-9 {
            continue;
        }
        if sign == 0.0 {
            sign = c.signum();
        } else if c.signum() != sign {
            return false;
        }
    }
    true
}

/// The layers under comp point `p`, front to back.
pub fn layers_at(ctx: &EvalCtx, p: [f64; 2]) -> Vec<LayerId> {
    selectable_layers(ctx.comp, ctx.time).filter(|l| layer_box(ctx, l).is_some_and(|b| point_in_quad(p, &b.quad))).map(|l| l.id).collect()
}

/// The layer a press at comp point `p` acts on: as in the viewer, a selected layer under the
/// point comes before the layers in front of it (so it can be dragged where they cover it),
/// otherwise the topmost one.
pub fn pick(ctx: &EvalCtx, p: [f64; 2], selected: &[LayerId]) -> Option<LayerId> {
    let hits = layers_at(ctx, p);
    hits.iter().find(|id| selected.contains(id)).or(hits.first()).copied()
}

/// Which axes handle `handle` scales: corners both, the top and bottom edges y, the right and
/// left edges x.
pub fn handle_axes(handle: usize) -> [bool; 2] {
    match handle {
        4 | 6 => [false, true],
        5 | 7 => [true, false],
        _ => [true, true],
    }
}

/// Handle `handle`'s point in layer pixels of the box `b` (handles as in [`HANDLES`]).
pub fn handle_point(b: [f64; 4], handle: usize) -> [f64; 2] {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    match handle {
        0 => [b[0], b[1]],
        1 => [b[2], b[1]],
        2 => [b[2], b[3]],
        3 => [b[0], b[3]],
        4 => [cx, b[1]],
        5 => [b[2], cy],
        6 => [cx, b[3]],
        _ => [b[0], cy],
    }
}

/// The scale ratio (x, y) that puts the point grabbed at `start` (layer pixels from the anchor)
/// under the pointer at `cur`. `uniform` (Shift on a corner) keeps the proportions, following
/// the pointer along the handle's diagonal. Past the anchor the layer flips; an axis whose
/// grabbed point sits on the anchor keeps its scale.
pub fn drag_ratio(start: [f64; 2], cur: [f64; 2], axes: [bool; 2], uniform: bool) -> [f64; 2] {
    if uniform {
        let d2 = start[0] * start[0] + start[1] * start[1];
        let f = if d2 > 1e-6 { (cur[0] * start[0] + cur[1] * start[1]) / d2 } else { 1.0 };
        return if f.is_finite() { [f, f] } else { [1.0, 1.0] };
    }
    [0, 1].map(|k| if axes[k] && start[k].abs() > 1e-3 && (cur[k] / start[k]).is_finite() { cur[k] / start[k] } else { 1.0 })
}

/// A move `d` (comp pixels), kept to the axis it moves along most with `shift`.
pub fn one_axis(d: [f64; 2], shift: bool) -> [f64; 2] {
    if !shift {
        d
    } else if d[0].abs() > d[1].abs() {
        [d[0], 0.0]
    } else {
        [0.0, d[1]]
    }
}

/// The Position change per comp pixel of pointer movement along x and along y: through the
/// parent for 2D layers, along the active camera's right and down at the layer's depth for 3D.
pub fn drag_axes(ctx: &EvalCtx, l: &Layer) -> ([f64; 3], [f64; 3]) {
    let parent = l.parent.and_then(|p| ctx.comp.layer(p));
    if !l.is_3d() {
        let inv = match parent {
            Some(p) => {
                let w = ctx.world_matrix(p);
                Mat3([[w.0[0][0], w.0[0][1], 0.0], [w.0[1][0], w.0[1][1], 0.0], [0.0, 0.0, 1.0]]).inverse().unwrap_or(Mat3::IDENTITY)
            }
            None => Mat3::IDENTITY,
        };
        let a = inv.apply_vec(vec2(1.0, 0.0));
        let b = inv.apply_vec(vec2(0.0, 1.0));
        return ([a.x, a.y, 0.0], [b.x, b.y, 0.0]);
    }
    let cam = effectcraft_render::three_d::active_camera(ctx);
    let anchor = l.transform().map(|tr| ctx.v3(l, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
    let wp = ctx.world_matrix(l).apply(anchor.into());
    let k = if cam.ortho { 1.0 / cam.zoom.max(1e-9) } else { cam.depth(wp).max(1.0) / cam.zoom.max(1e-9) };
    let pinv = match parent {
        Some(p) => ctx.world_matrix(p).inverse().unwrap_or_default(),
        None => Mat4::IDENTITY,
    };
    let a = pinv.apply_vec(cam.right() * k);
    let b = pinv.apply_vec(cam.down() * k);
    ([a.x, a.y, a.z], [b.x, b.y, b.z])
}

/// A body drag: the layers it moves with their Position as it began and their [`drag_axes`].
#[derive(Clone, Debug, PartialEq)]
pub struct MoveDrag {
    pub start: [f64; 2],
    pub layers: Vec<(LayerId, [f64; 3], [f64; 3], [f64; 3])>,
}

impl MoveDrag {
    /// Begin moving `layers` (the unlocked ones with a transform) from comp point `start`.
    pub fn begin(ctx: &EvalCtx, layers: &[LayerId], start: [f64; 2]) -> Option<MoveDrag> {
        let layers: Vec<_> = layers
            .iter()
            .filter_map(|id| ctx.comp.layer(*id))
            .filter(|l| !l.switches.locked)
            .filter_map(|l| {
                let p0 = ctx.v3(l, l.transform()?, "position", [0.0; 3]);
                let (ax, ay) = drag_axes(ctx, l);
                Some((l.id, p0, ax, ay))
            })
            .collect();
        (!layers.is_empty()).then_some(MoveDrag { start, layers })
    }

    /// Each layer's Position with the pointer at comp point `cur` (`shift`: along one axis).
    pub fn values(&self, cur: [f64; 2], shift: bool) -> Vec<(LayerId, [f64; 3])> {
        let d = one_axis([cur[0] - self.start[0], cur[1] - self.start[1]], shift);
        if d.iter().any(|v| !v.is_finite()) {
            return vec![];
        }
        self.layers.iter().map(|(id, p0, ax, ay)| (*id, [0, 1, 2].map(|k| p0[k] + ax[k] * d[0] + ay[k] * d[1]))).collect()
    }
}

/// A handle drag: the layer, its Scale as it began, where it was grabbed (layer pixels from the
/// anchor), the axes the handle scales, comp → layer, and the other selected layers' Scale
/// (they scale by the same ratio, as in the viewer).
#[derive(Clone, Debug, PartialEq)]
pub struct ScaleDrag {
    pub layer: LayerId,
    pub start_scale: [f64; 3],
    pub start_local: [f64; 2],
    pub anchor: [f64; 2],
    pub axes: [bool; 2],
    pub inv: Mat3,
    pub others: Vec<(LayerId, [f64; 3])>,
}

impl ScaleDrag {
    /// Begin dragging handle `handle` of `layer`, pressed at comp point `press`; `others`: the
    /// other selected layers.
    pub fn begin(ctx: &EvalCtx, layer: LayerId, handle: usize, press: [f64; 2], others: &[LayerId]) -> Option<ScaleDrag> {
        let l = ctx.comp.layer(layer).filter(|l| !l.switches.locked)?;
        let tr = l.transform()?;
        let (l2c, _) = ctx.layer_to_comp(l);
        let inv = l2c.inverse()?;
        let a = ctx.v3(l, tr, "anchor", [0.0; 3]);
        let lp = inv.apply(vec2(press[0], press[1]));
        let others = others
            .iter()
            .filter(|id| **id != layer)
            .filter_map(|id| ctx.comp.layer(*id))
            .filter(|o| !o.switches.locked)
            .filter_map(|o| Some((o.id, ctx.v3(o, o.transform()?, "scale", [100.0; 3]))))
            .collect();
        Some(ScaleDrag {
            layer,
            start_scale: ctx.v3(l, tr, "scale", [100.0; 3]),
            start_local: [lp.x - a[0], lp.y - a[1]],
            anchor: [a[0], a[1]],
            axes: handle_axes(handle),
            inv,
            others,
        })
    }

    /// Each layer's Scale with the pointer at comp point `cur`; `shift` on a corner keeps the
    /// proportions.
    pub fn values(&self, cur: [f64; 2], shift: bool) -> Vec<(LayerId, [f64; 3])> {
        let lp = self.inv.apply(vec2(cur[0], cur[1]));
        let uniform = shift && self.axes == [true, true];
        let f = drag_ratio(self.start_local, [lp.x - self.anchor[0], lp.y - self.anchor[1]], self.axes, uniform);
        std::iter::once((self.layer, self.start_scale)).chain(self.others.iter().copied()).map(|(id, s0)| (id, [s0[0] * f[0], s0[1] * f[1], s0[2]])).collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Session;

    /// A 640×360 comp with a 100×100 solid in the middle (box 270,130 – 370,230).
    fn comp() -> (Session, LayerId) {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "Main", "width": 640, "height": 360, "frameRate": 30, "duration": 10})).unwrap();
        let id = s.execute("layer.newSolid", json!({"name": "A", "width": 100, "height": 100})).unwrap()["layer"].as_u64().unwrap();
        (s, LayerId(id))
    }

    fn ctx(s: &Session) -> EvalCtx<'_> {
        EvalCtx::new(&s.project, s.active_comp_id().unwrap(), s.active_comp().unwrap(), s.time())
    }

    #[test]
    fn a_layer_box_and_its_handles_are_in_comp_pixels() {
        let (s, id) = comp();
        let c = ctx(&s);
        let b = layer_box(&c, c.comp.layer(id).unwrap()).unwrap();
        assert_eq!(b.quad, [[270.0, 130.0], [370.0, 130.0], [370.0, 230.0], [270.0, 230.0]]);
        assert_eq!(b.handle(2), [370.0, 230.0]);
        assert_eq!(b.handle(5), [370.0, 180.0]);
        assert_eq!(b.handles().len(), HANDLES);
    }

    #[test]
    fn a_press_picks_the_topmost_layer_or_the_selected_one_under_it() {
        let (mut s, a) = comp();
        let b = LayerId(s.execute("layer.newSolid", json!({"name": "B", "width": 100, "height": 100})).unwrap()["layer"].as_u64().unwrap());
        let c = ctx(&s);
        assert_eq!(pick(&c, [320.0, 180.0], &[]), Some(b));
        assert_eq!(pick(&c, [320.0, 180.0], &[a]), Some(a));
        assert_eq!(pick(&c, [10.0, 10.0], &[]), None);
        s.execute("layer.setSwitch", json!({"layers": [b.0], "switch": "locked", "value": true})).unwrap();
        assert_eq!(pick(&ctx(&s), [320.0, 180.0], &[]), Some(a));
    }

    #[test]
    fn a_body_drag_moves_position_by_the_comp_distance_and_shift_keeps_one_axis() {
        let (s, id) = comp();
        let c = ctx(&s);
        let d = MoveDrag::begin(&c, &[id], [320.0, 180.0]).unwrap();
        assert_eq!(d.values([330.0, 175.0], false), vec![(id, [330.0, 175.0, 0.0])]);
        assert_eq!(d.values([330.0, 175.0], true), vec![(id, [330.0, 180.0, 0.0])]);
        assert!(d.values([f64::NAN, 0.0], false).is_empty());
    }

    #[test]
    fn a_handle_drag_scales_and_shift_keeps_proportions() {
        let (s, id) = comp();
        let c = ctx(&s);
        // The bottom-right corner, dragged 50 px right: x doubles, y stays.
        let d = ScaleDrag::begin(&c, id, 2, [370.0, 230.0], &[]).unwrap();
        assert_eq!(d.values([420.0, 230.0], false), vec![(id, [200.0, 100.0, 100.0])]);
        // Shift: both follow along the diagonal.
        let v = d.values([420.0, 230.0], true);
        assert!((v[0].1[0] - v[0].1[1]).abs() < 1e-9 && v[0].1[0] > 100.0);
        // The right edge scales x only.
        let e = ScaleDrag::begin(&c, id, 5, [370.0, 180.0], &[]).unwrap();
        assert_eq!(e.values([395.0, 300.0], false), vec![(id, [150.0, 100.0, 100.0])]);
    }

    #[test]
    fn drag_ratios_never_go_non_finite() {
        assert_eq!(drag_ratio([0.0, 0.0], [5.0, 5.0], [true, true], false), [1.0, 1.0]);
        assert_eq!(drag_ratio([0.0, 0.0], [5.0, 5.0], [true, true], true), [1.0, 1.0]);
        assert_eq!(drag_ratio([10.0, 10.0], [f64::INFINITY, 5.0], [true, true], false), [1.0, 0.5]);
    }
}
