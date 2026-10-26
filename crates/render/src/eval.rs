//! Property evaluation (keyframes + expressions) and layer transforms.
//!
//! Keyframe times are stored in **layer time** (they move with the layer), so evaluating a
//! property at comp time `t` first maps `t` through the layer's start time and stretch.

use effectcraft_geom::{Mat3, Mat4, Vec3, vec2, vec3};
use effectcraft_keyframe::Value;
use effectcraft_project::{Comp, ItemId, Layer, LayerId, LayerSource, Project, PropGroup, Property};
use effectcraft_time::Tick;

/// Expression evaluation hook (implemented by `effectcraft-expr`).
pub trait ExprHost: Send + Sync {
    /// Evaluate the expression of `prop` (on `layer`, comp time `t`), given the keyframed value.
    fn eval(&self, ctx: &EvalCtx, layer: &Layer, prop: &Property, value: &Value) -> Result<Value, String>;
    /// Evaluate a text Expression Selector's Amount expression (on `prop`) for one unit:
    /// `textIndex` (1-based), `textTotal` and `selectorValue` (percent, per dimension) are
    /// defined. Returns the amount in percent per dimension.
    fn eval_text_selector(
        &self,
        _ctx: &EvalCtx,
        _layer: &Layer,
        _prop: &Property,
        _index: usize,
        _total: usize,
        _selector: [f64; 3],
    ) -> Result<[f64; 3], String> {
        Err("no expression engine".into())
    }
}

#[derive(Clone, Copy)]
pub struct EvalCtx<'a> {
    pub project: &'a Project,
    pub comp_id: ItemId,
    pub comp: &'a Comp,
    /// Comp time.
    pub time: Tick,
    pub expr: Option<&'a dyn ExprHost>,
    /// Footage frames for expressions that read rendered pixels (`sampleImage`): the renderer's
    /// own source while rendering; `None` renders footage as transparent.
    pub footage: Option<&'a dyn crate::FootageSource>,
}

impl<'a> EvalCtx<'a> {
    pub fn new(project: &'a Project, comp_id: ItemId, comp: &'a Comp, time: Tick) -> EvalCtx<'a> {
        EvalCtx { project, comp_id, comp, time, expr: None, footage: None }
    }
    pub fn at(&self, time: Tick) -> EvalCtx<'a> {
        EvalCtx { time, ..*self }
    }
    /// Value of a property at the context time (keyframes, then expression).
    pub fn value(&self, layer: &Layer, prop: &Property) -> Value {
        let _profile = crate::profile::span("evaluation");
        // Separated Position reads as the combination of X/Y/Z Position.
        if prop.match_id == "position"
            && let Some(tr) = layer.transform()
            && let Some(px) = tr.get("positionX")
            && tr.get("position").is_some_and(|p| p.uid == prop.uid)
        {
            let z = tr.get("positionZ").map(|p| self.value(layer, p).as_f64()).unwrap_or(0.0);
            let y = tr.get("positionY").map(|p| self.value(layer, p).as_f64()).unwrap_or(0.0);
            return Value::Vec3([self.value(layer, px).as_f64(), y, z]);
        }
        let lt = layer.layer_time(self.time);
        let v = prop.value_at(lt);
        if prop.has_expression()
            && let Some(h) = self.expr
        {
            let _expression = crate::profile::span("expressions");
            return h.eval(self, layer, prop, &v).unwrap_or(v);
        }
        v
    }
    pub fn group_value(&self, layer: &Layer, g: &PropGroup, m: &str) -> Option<Value> {
        g.get(m).map(|p| self.value(layer, p))
    }
    pub fn f(&self, layer: &Layer, g: &PropGroup, m: &str, d: f64) -> f64 {
        self.group_value(layer, g, m).map(|v| v.as_f64()).unwrap_or(d)
    }
    pub fn v2(&self, layer: &Layer, g: &PropGroup, m: &str, d: [f64; 2]) -> [f64; 2] {
        self.group_value(layer, g, m).map(|v| v.as_vec2()).unwrap_or(d)
    }
    pub fn v3(&self, layer: &Layer, g: &PropGroup, m: &str, d: [f64; 3]) -> [f64; 3] {
        self.group_value(layer, g, m).map(|v| v.as_vec3()).unwrap_or(d)
    }
    pub fn color(&self, layer: &Layer, g: &PropGroup, m: &str) -> [f32; 4] {
        self.group_value(layer, g, m).map(|v| v.as_color()).unwrap_or([1.0; 4])
    }
    pub fn e(&self, layer: &Layer, g: &PropGroup, m: &str) -> u32 {
        self.group_value(layer, g, m).map(|v| v.as_enum()).unwrap_or(0)
    }
    pub fn b(&self, layer: &Layer, g: &PropGroup, m: &str) -> bool {
        self.group_value(layer, g, m).map(|v| v.as_bool()).unwrap_or(false)
    }
    /// Layer Position, honouring Separate Dimensions (X/Y/Z Position properties).
    pub fn position(&self, layer: &Layer, tr: &PropGroup) -> [f64; 3] {
        match tr.get("positionX") {
            Some(px) => [self.value(layer, px).as_f64(), self.f(layer, tr, "positionY", 0.0), self.f(layer, tr, "positionZ", 0.0)],
            None => self.v3(layer, tr, "position", [0.0; 3]),
        }
    }
    /// Source time of a layer at the context time: Time Remap's value when enabled, else the
    /// (stretch-aware) layer time.
    pub fn source_time(&self, layer: &Layer) -> Tick {
        if let Some(tr) = layer.props.get("timeRemap") {
            return Tick::from_seconds_f64(self.value(layer, tr).as_f64());
        }
        // Responsive Design — Time: a time-stretched precomp keeps its protected regions at
        // their original speed.
        if let LayerSource::Comp { item } = layer.source
            && (layer.stretch - 100.0).abs() > 1e-9
            && let Some(nc) = self.project.comp(item)
            && let Some(t) = responsive_source_time(nc, layer.stretch / 100.0, (self.time - layer.start_time).seconds())
        {
            return t;
        }
        layer.layer_time(self.time)
    }
    /// The time a precomp layer's nested comp is shown at: [`Self::source_time`], floored to
    /// the nested comp's own frames when it preserves its frame rate (Composition Settings ▸
    /// Preserve frame rate when nested or in render queue).
    pub fn nested_time(&self, layer: &Layer) -> Tick {
        let t = self.source_time(layer);
        match layer.source {
            LayerSource::Comp { item } => match self.project.comp(item) {
                Some(nc) if nc.preserve_frame_rate => nc.frame_rate.tick_of(crate::frame_position(t, nc.frame_rate).0),
                _ => t,
            },
            _ => t,
        }
    }
    pub fn layer(&self, id: LayerId) -> Option<&'a Layer> {
        self.comp.layer(id)
    }

    /// Local (parent-space) transform of a layer.
    pub fn local_matrix(&self, layer: &Layer) -> Mat4 {
        self.local_matrix_par(layer, 1.0)
    }

    /// Source pixel aspect relative to the comp's: non-square footage (or solids, or precomps
    /// with another pixel aspect) is stretched horizontally by this factor in the comp.
    pub fn par_ratio(&self, layer: &Layer) -> f64 {
        let par = match &layer.source {
            LayerSource::Footage { item } | LayerSource::Solid { item } | LayerSource::Comp { item } => match self.project.item(*item).map(|i| &i.kind) {
                Some(effectcraft_project::ItemKind::Footage(f)) => f.pixel_aspect,
                Some(effectcraft_project::ItemKind::Solid(s)) => s.pixel_aspect,
                Some(effectcraft_project::ItemKind::Comp(c)) => c.pixel_aspect,
                _ => 1.0,
            },
            _ => 1.0,
        };
        let r = par / if self.comp.pixel_aspect > 0.0 { self.comp.pixel_aspect } else { 1.0 };
        if r.is_finite() && r > 0.0 && (r - 1.0).abs() > 1e-9 { r } else { 1.0 }
    }

    /// [`Self::local_matrix`] with the source's pixel aspect folded into the scale (`par`; 1 for
    /// a parent's matrix, which children don't inherit).
    fn local_matrix_par(&self, layer: &Layer, par: f64) -> Mat4 {
        let Some(tr) = layer.transform() else { return Mat4::IDENTITY };
        let three = layer.is_3d();
        let pos = self.position(layer, tr);
        let rz = self.f(layer, tr, "rotation", 0.0);
        if layer.is_camera() || layer.is_light() {
            // Children of cameras/lights follow their position and full rotation.
            return Mat4::translate(Vec3::from(pos)) * crate::three_d::camera::rig_rotation(self, layer);
        }
        let anchor = self.v3(layer, tr, "anchor", [0.0; 3]);
        let mut scale = self.v3(layer, tr, "scale", [100.0; 3]);
        scale[0] *= par;
        if !three {
            scale[2] = 100.0;
            let a = Vec3::from([anchor[0], anchor[1], 0.0]);
            let p = Vec3::from([pos[0], pos[1], 0.0]);
            let rz = rz + crate::three_d::camera::auto_orient_2d(self, layer);
            return Mat4::layer_3d(a, p, Vec3::from(scale), Vec3::ZERO, vec3(0.0, 0.0, rz));
        }
        let rx = self.f(layer, tr, "rotationX", 0.0);
        let ry = self.f(layer, tr, "rotationY", 0.0);
        let p = Vec3::from(pos);
        // Auto-orient (along path / towards camera) replaces Orientation.
        let orient = crate::three_d::camera::auto_orient_3d(self, layer, p).unwrap_or_else(|| {
            let o = Vec3::from(self.v3(layer, tr, "orientation", [0.0; 3]));
            if crate::three_d::adv::ae_model_axes(self, layer) { Mat4::rotate_x(o.x) * Mat4::rotate_y(o.y) * Mat4::rotate_z(o.z) } else { Mat4::orientation(o) }
        });
        // AE composes the separate X/Y/Z Rotation controls as Rx·Ry·Rz.
        Mat4::translate(p)
            * orient
            * Mat4::rotate_x(rx)
            * Mat4::rotate_y(ry)
            * Mat4::rotate_z(rz)
            * Mat4::scale(Vec3::from(scale) / 100.0)
            * Mat4::translate(-Vec3::from(anchor))
    }

    /// Layer space → comp (world) space, including parents. Inside [`with_transform_memo`] the
    /// parents' local matrices are evaluated once per (layer, time) and found by id in O(1).
    pub fn world_matrix(&self, layer: &Layer) -> Mat4 {
        let mut m = self.local_matrix_par(layer, self.par_ratio(layer));
        let mut cur = layer.parent;
        let mut guard = 0;
        while let Some(pid) = cur {
            guard += 1;
            if guard > 64 {
                break;
            }
            // Same product order as without the memo: bit-identical.
            let Some((local, next)) = self.parent_local(pid) else { break };
            m = local * m;
            cur = next;
        }
        m
    }

    /// [`Self::world_matrix`] with the parents' followed Transform effects (see
    /// [`Self::follow_fx`]) evaluated in `fx` instead (the same comp at another time): a child
    /// whose own motion blur is off still moves with them across their shutter.
    pub fn world_matrix_fx(&self, layer: &Layer, fx: &EvalCtx) -> Mat4 {
        let mut m = self.local_matrix_par(layer, self.par_ratio(layer));
        let mut cur = layer.parent;
        let mut guard = 0;
        while let Some(pid) = cur {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(p) = self.comp.layer(pid) else { break };
            let follow = fx.follow_fx(p).map_or(Mat4::IDENTITY, |f| f.0);
            m = self.local_matrix(p) * follow * m;
            cur = p.parent;
        }
        m
    }

    /// [`Self::follow_fx`]'s matrix as a 2D affine (layer pixels): `None` when nothing is followed.
    pub fn follow_fx_2d(&self, layer: &Layer) -> Option<Mat3> {
        let m = self.follow_fx(layer)?.0.0;
        Some(Mat3([[m[0][0], m[0][1], m[0][3]], [m[1][0], m[1][1], m[1][3]], [0.0, 0.0, 1.0]]))
    }

    /// The space a layer's children live in: its [`Self::world_matrix`] then the Transform
    /// effects they follow (see [`Self::follow_fx`]).
    pub fn child_space(&self, layer: &Layer) -> Mat4 {
        let m = self.world_matrix(layer);
        self.follow_fx(layer).map_or(m, |f| m * f.0)
    }

    /// The layer's Transform effects that its children follow (Children Follow, see
    /// `effectcraft_effects::transform_follow`), in stack order, as one layer-space matrix, and
    /// the opacity factor they pass on. `None` when no enabled Transform effect passes anything.
    pub fn follow_fx(&self, layer: &Layer) -> Option<(Mat4, f64)> {
        if !layer.switches.effects || layer.switches.adjustment {
            return None;
        }
        let fx = layer.effects()?;
        let is_transform =
            |g: &&PropGroup| g.enabled && matches!(&g.kind, effectcraft_project::GroupKind::Effect { effect } if effect == "ec.distort.transform");
        if !fx.groups().any(|g| is_transform(&g)) {
            return None;
        }
        // Effect points are measured from the effect space's corner (see `effect_bounds`).
        let o = self.effect_bounds(layer).1;
        let mut out: Option<(Mat4, f64)> = None;
        for g in fx.groups().filter(is_transform) {
            let pr = effectcraft_effects::flatten_params(g, &mut |p| self.value(layer, p));
            let [a, q] = [pr.v2("anchor"), pr.v2("position")];
            let Some((m, op)) = effectcraft_effects::transform_follow(&pr, (a[0] + o[0], a[1] + o[1]), (q[0] + o[0], q[1] + o[1])) else { continue };
            let (prev, prev_op) = out.unwrap_or((Mat4::IDENTITY, 1.0));
            // Later effects transform the earlier ones' result.
            out = Some((Mat4::from_mat3_affine(&m) * prev, prev_op * op));
        }
        out
    }

    /// A parent's local matrix and its own parent (memoised inside [`with_transform_memo`]).
    fn parent_local(&self, pid: LayerId) -> Option<(Mat4, Option<LayerId>)> {
        let key = memo::key(self, pid);
        if let Some(k) = &key
            && let Some(hit) = memo::get(k)
        {
            return Some(hit);
        }
        let p = key.as_ref().and_then(|k| memo::find(self, k)).unwrap_or_else(|| self.comp.layer(pid))?;
        // Children follow the parent's Transform effects too (Children Follow).
        let follow = self.follow_fx(p).map_or(Mat4::IDENTITY, |f| f.0);
        let v = (self.local_matrix(p) * follow, p.parent);
        if let Some(k) = key {
            memo::put(k, v);
        }
        Some(v)
    }

    /// The layer's Opacity, times that of the parents' Transform effects it follows (Children
    /// Follow Opacity).
    pub fn opacity(&self, layer: &Layer) -> f64 {
        let own = layer.transform().map(|tr| self.f(layer, tr, "opacity", 100.0)).unwrap_or(100.0) / 100.0;
        let mut k = 1.0;
        let mut cur = layer.parent;
        let mut guard = 0;
        while let Some(pid) = cur {
            guard += 1;
            let Some(p) = self.comp.layer(pid).filter(|_| guard <= 64) else { break };
            k *= self.follow_fx(p).map_or(1.0, |f| f.1);
            cur = p.parent;
        }
        own * k
    }

    /// The comp's active camera at this time: (view-projection to comp pixels, eye, zoom).
    pub fn camera(&self) -> (Mat4, Vec3, f64) {
        let c = crate::three_d::active_camera(self);
        (c.projection(self.comp.width as f64, self.comp.height as f64), c.eye, c.zoom)
    }

    /// Layer space → comp pixel space as a 2D projective matrix, plus camera-space depth of the
    /// layer's anchor (for 3D sorting). 2D layers ignore the camera.
    pub fn layer_to_comp(&self, layer: &Layer) -> (Mat3, f64) {
        self.layer_to_comp_of(layer, self.world_matrix(layer))
    }

    /// [`Self::layer_to_comp`] with the parents' followed Transform effects evaluated in `fx`
    /// (see [`Self::world_matrix_fx`]).
    pub fn layer_to_comp_fx(&self, layer: &Layer, fx: &EvalCtx) -> (Mat3, f64) {
        self.layer_to_comp_of(layer, self.world_matrix_fx(layer, fx))
    }

    fn layer_to_comp_of(&self, layer: &Layer, world: Mat4) -> (Mat3, f64) {
        if layer.is_3d() {
            let (cam, _, _) = self.camera();
            let full = cam * world;
            let anchor = layer.transform().map(|tr| self.v3(layer, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
            let a = world.apply(Vec3::from(anchor));
            let depth = cam.0[3][0] * a.x + cam.0[3][1] * a.y + cam.0[3][2] * a.z + cam.0[3][3];
            (full.plane_to_mat3(), depth)
        } else {
            let m = &world.0;
            (Mat3([[m[0][0], m[0][1], m[0][3]], [m[1][0], m[1][1], m[1][3]], [0.0, 0.0, 1.0]]), 0.0)
        }
    }
    /// [`effect_bounds`] of a layer of this comp at this time. A shape layer's comp-sized bounds
    /// are centred on its anchor point rather than its origin: After Effects renders a shape
    /// layer's effects on a comp-sized canvas centred on the anchor, so effect points (Transform
    /// anchors, ramp / 4-colour points, light-sweep centres) are measured from that canvas's
    /// corner (SceneYard Calendar Page Tear: the pages hinged ~1000 px away otherwise).
    pub fn effect_bounds(&self, layer: &Layer) -> ([f64; 2], [f64; 2]) {
        let (size, origin) = effect_bounds(self.project, self.comp, layer);
        if !matches!(layer.source, LayerSource::Shape) {
            return (size, origin);
        }
        let a = layer.transform().map(|tr| self.v3(layer, tr, "anchor", [0.0; 3])).unwrap_or([0.0; 3]);
        (size, [origin[0] + a[0], origin[1] + a[1]])
    }
    /// Effect space (see [`effect_bounds`]) → comp matrix.
    pub fn effect_to_comp(&self, layer: &Layer) -> Mat3 {
        let o = self.effect_bounds(layer).1;
        self.layer_to_comp(layer).0 * Mat3::translate(vec2(o[0], o[1]))
    }
}

/// Run `f` with the parents' local matrices memoised per (layer, time) for `project` on this
/// thread (see [`EvalCtx::world_matrix`]): for one frame's evaluation, while `project` cannot
/// change. Nested calls for the same project share the memo; another project (a nested comp's
/// Essential Properties copy) gets its own, dropped with it. Contexts of other projects or of
/// comps outside `project` are evaluated as without the memo.
pub fn with_transform_memo<R>(project: &Project, f: impl FnOnce() -> R) -> R {
    let _scope = memo::Scope::enter(project);
    f()
}

/// The transform memo behind [`with_transform_memo`].
mod memo {
    use super::EvalCtx;
    use effectcraft_geom::Mat4;
    use effectcraft_project::{Layer, LayerId, Project};
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A parent's local matrix at one time, seen through one context: comp (address), layer,
    /// time, expression host and footage source (addresses).
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct Key {
        comp: usize,
        layer: LayerId,
        time: i64,
        expr: usize,
        footage: usize,
    }

    struct Frame {
        /// The project the memo is for (it outlives the frame: the scope borrows it).
        project: usize,
        /// Whether a comp address (with its id) is that project's comp.
        comps: HashMap<(usize, u64), bool>,
        /// Layer id → index in each comp.
        index: HashMap<usize, HashMap<LayerId, usize>>,
        locals: HashMap<Key, (Mat4, Option<LayerId>)>,
    }

    thread_local! {
        static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    }

    /// An open memo scope: pushes a frame unless the innermost one is for the same project, pops
    /// it when dropped (also when unwinding).
    pub(super) struct Scope {
        pushed: bool,
    }

    impl Scope {
        pub(super) fn enter(project: &Project) -> Scope {
            #[cfg(test)]
            if super::tests::NO_MEMO.with(std::cell::Cell::get) {
                return Scope { pushed: false };
            }
            let addr = project as *const Project as usize;
            let pushed = FRAMES
                .try_with(|f| {
                    let Ok(mut v) = f.try_borrow_mut() else { return false };
                    if v.last().is_some_and(|fr| fr.project == addr) {
                        return false;
                    }
                    v.push(Frame { project: addr, comps: HashMap::new(), index: HashMap::new(), locals: HashMap::new() });
                    true
                })
                .unwrap_or(false);
            Scope { pushed }
        }
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            if self.pushed {
                let _ = FRAMES.try_with(|f| f.try_borrow_mut().map(|mut v| v.pop()));
            }
        }
    }

    fn addr<T: ?Sized>(r: Option<&T>) -> usize {
        r.map_or(0, |r| r as *const T as *const () as usize)
    }

    /// The memo key of `pid` in `ctx`, when a memo is open for `ctx`'s project and `ctx.comp`
    /// is that project's comp.
    pub(super) fn key(ctx: &EvalCtx, pid: LayerId) -> Option<Key> {
        let comp = ctx.comp as *const _ as usize;
        let project = ctx.project as *const Project as usize;
        let ok = FRAMES
            .try_with(|f| {
                let Ok(mut v) = f.try_borrow_mut() else { return false };
                let Some(fr) = v.last_mut().filter(|fr| fr.project == project) else { return false };
                *fr.comps.entry((comp, ctx.comp_id.0)).or_insert_with(|| ctx.project.comp(ctx.comp_id).is_some_and(|c| std::ptr::eq(c, ctx.comp)))
            })
            .unwrap_or(false);
        ok.then(|| Key { comp, layer: pid, time: ctx.time.0, expr: addr(ctx.expr), footage: addr(ctx.footage) })
    }

    #[cfg(test)]
    impl Key {
        pub(super) fn layer_id(&self) -> LayerId {
            self.layer
        }
    }

    pub(super) fn get(k: &Key) -> Option<(Mat4, Option<LayerId>)> {
        FRAMES.try_with(|f| f.try_borrow().ok().and_then(|v| v.last().and_then(|fr| fr.locals.get(k).copied()))).ok().flatten()
    }

    pub(super) fn put(k: Key, v: (Mat4, Option<LayerId>)) {
        let _ = FRAMES.try_with(|f| {
            if let Ok(mut fs) = f.try_borrow_mut()
                && let Some(fr) = fs.last_mut()
            {
                fr.locals.insert(k, v);
            }
        });
    }

    /// The layer `k` names in `ctx.comp`, through the comp's id → index map (built once).
    pub(super) fn find<'a>(ctx: &EvalCtx<'a>, k: &Key) -> Option<Option<&'a Layer>> {
        let i = FRAMES
            .try_with(|f| {
                let mut fs = f.try_borrow_mut().ok()?;
                let fr = fs.last_mut()?;
                let index = fr.index.entry(k.comp).or_insert_with(|| ctx.comp.layers.iter().enumerate().rev().map(|(i, l)| (l.id, i)).collect());
                Some(index.get(&k.layer).copied())
            })
            .ok()
            .flatten()?;
        Some(i.and_then(|i| ctx.comp.layers.get(i)))
    }
}

/// The protected regions of a comp (Responsive Design — Time markers), merged, sorted and
/// clamped to the comp, in seconds.
pub fn protected_regions(comp: &Comp) -> Vec<(f64, f64)> {
    let d = comp.duration.seconds();
    let mut v: Vec<(f64, f64)> = comp
        .markers
        .iter()
        .filter(|m| m.protected && m.duration > Tick::ZERO)
        .map(|m| (m.time.seconds().clamp(0.0, d), (m.time + m.duration).seconds().clamp(0.0, d)))
        .filter(|(a, b)| b > a)
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = vec![];
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Source time of a precomp time-stretched by `stretch` (1 = 100%), `elapsed` comp seconds
/// after the layer's start, with Responsive Design — Time: protected regions play at their
/// original speed and the unprotected parts absorb the whole stretch. `None` when the comp has
/// no protected regions or the stretch can't be absorbed (reversed, or shorter than the
/// protected regions), so the plain stretch applies.
pub fn responsive_source_time(comp: &Comp, stretch: f64, elapsed: f64) -> Option<Tick> {
    let regions = protected_regions(comp);
    if regions.is_empty() || stretch <= 0.0 {
        return None;
    }
    let d = comp.duration.seconds();
    let lp: f64 = regions.iter().map(|(a, b)| b - a).sum();
    let lu = d - lp;
    let k = if lu > 1e-9 { (d * stretch - lp) / lu } else { 1.0 };
    if k <= 1e-9 {
        return None;
    }
    if elapsed < 0.0 {
        return Some(Tick::from_seconds_f64(elapsed / k));
    }
    // Segments in source order: (start, end, protected).
    let mut segs = vec![];
    let mut at = 0.0;
    for (a, b) in &regions {
        if *a > at {
            segs.push((at, *a, false));
        }
        segs.push((*a, *b, true));
        at = *b;
    }
    if d > at {
        segs.push((at, d, false));
    }
    let mut pos = 0.0;
    for (a, b, prot) in segs {
        let speed = if prot { 1.0 } else { k };
        let len = (b - a) * speed;
        if elapsed < pos + len {
            return Some(Tick::from_seconds_f64(a + (elapsed - pos) / speed));
        }
        pos += len;
    }
    Some(Tick::from_seconds_f64(d + (elapsed - pos) / k))
}

/// A layer's effect bounds ([`effectcraft_effects::EffectCtx::layer_size`]) and their top-left
/// corner in layer coordinates: the source rectangle at (0, 0), or for layers without one
/// (shape, text) a comp-sized rectangle centred on the layer's origin, which their content
/// surrounds. Effects work in **effect space**, measured from that corner (After Effects' layer
/// space for effect points: a shape layer's default effect point is its bounds' centre).
pub fn effect_bounds(project: &Project, comp: &Comp, layer: &Layer) -> ([f64; 2], [f64; 2]) {
    match source_size(project, layer) {
        (0, _) => {
            let (w, h) = (comp.width as f64, comp.height as f64);
            ([w, h], [-w / 2.0, -h / 2.0])
        }
        (w, h) => ([w as f64, h as f64], [0.0; 2]),
    }
}

/// Size of a layer's source in layer pixels.
pub fn source_size(project: &Project, layer: &Layer) -> (u32, u32) {
    match &layer.source {
        LayerSource::Footage { item } | LayerSource::Comp { item } | LayerSource::Solid { item } => {
            project.item(*item).and_then(|i| i.dimensions()).unwrap_or((0, 0))
        }
        LayerSource::Null => (100, 100),
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use effectcraft_color::Label;
    use effectcraft_keyframe::{Keyframe, Value};
    use effectcraft_project::build;
    use effectcraft_project::{BitDepth, Comp, ItemId, ItemKind, Layer, LayerId, LayerSource, Project, Solid};
    use effectcraft_time::{FrameRate, Tick};

    use super::{EvalCtx, with_transform_memo};
    use crate::{NoFootage, RenderOpts, Renderer};

    thread_local! {
        /// Evaluate without the transform memo (the path before it).
        pub(super) static NO_MEMO: Cell<bool> = const { Cell::new(false) };
    }

    fn keys(l: &mut Layer, path: &str, a: Value, b: Value) {
        l.props.prop_mut(path).unwrap().keys = vec![Keyframe::new(Tick::ZERO, a), Keyframe::new(Tick::from_seconds_f64(1.0), b)];
    }

    /// Two animated nulls in a chain, three solids parented to them (one to a missing layer).
    fn chain(p: &mut Project, comp: &Comp) -> Vec<Layer> {
        let mut n1 = build::layer(p, comp, "N1", LayerSource::Null, (100, 100), None);
        keys(&mut n1, "transform/rotation", Value::Scalar(0.0), Value::Scalar(37.0));
        keys(&mut n1, "transform/position", Value::Vec3([20.0, 30.0, 0.0]), Value::Vec3([80.0, 40.0, 0.0]));
        let mut n2 = build::layer(p, comp, "N2", LayerSource::Null, (100, 100), None);
        keys(&mut n2, "transform/scale", Value::Vec3([100.0, 100.0, 100.0]), Value::Vec3([70.0, 130.0, 100.0]));
        n2.props.prop_mut("transform/position").unwrap().value = Value::Vec3([33.3, 17.1, 0.0]);
        n2.parent = Some(n1.id);
        let mut v = vec![];
        for (i, parent) in [Some(n2.id), Some(n2.id), Some(LayerId(987_654))].into_iter().enumerate() {
            let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: [0.2 * i as f32, 0.6, 0.9], width: 30, height: 20, pixel_aspect: 1.0 }));
            let mut l = build::layer(p, comp, "S", LayerSource::Solid { item: sid }, (30, 20), None);
            l.props.prop_mut("transform/position").unwrap().value = Value::Vec3([10.0 + 25.0 * i as f64, 12.5, 0.0]);
            keys(&mut l, "transform/rotation", Value::Scalar(3.0 * i as f64), Value::Scalar(-20.0));
            l.parent = parent;
            v.push(l);
        }
        v.push(n2);
        v.push(n1);
        v
    }

    fn bits(m: &effectcraft_geom::Mat4) -> Vec<u64> {
        m.0.iter().flatten().map(|v| v.to_bits()).collect()
    }

    /// World matrices through the memo (first evaluation and cached) are bit-identical.
    #[test]
    fn memoised_world_matrices_are_bit_identical() {
        let mut p = Project::default();
        let comp = Comp::new(200, 100, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
        let layers = chain(&mut p, &comp);
        let mut c = comp;
        c.layers = layers.into();
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(c.into()));
        let comp = p.comp(cid).unwrap();
        for t in [0.0, 0.37, 1.2] {
            let ctx = EvalCtx::new(&p, cid, comp, Tick::from_seconds_f64(t));
            let plain: Vec<Vec<u64>> = comp.layers.iter().map(|l| bits(&ctx.world_matrix(l))).collect();
            with_transform_memo(&p, || {
                for _ in 0..2 {
                    let memo: Vec<Vec<u64>> = comp.layers.iter().map(|l| bits(&ctx.world_matrix(l))).collect();
                    assert_eq!(plain, memo, "t {t}");
                }
                // The parents are cached, keyed by time.
                let k = super::memo::key(&ctx, comp.layers.get(3).unwrap().id).unwrap();
                assert!(super::memo::get(&k).is_some());
                assert!(super::memo::key(&ctx.at(Tick::from_seconds_f64(t + 0.5)), k.layer_id()).is_some_and(|k| super::memo::get(&k).is_none()));
            });
        }
        // A copy of the comp outside the project is evaluated without the memo.
        let copy = comp.clone();
        let ctx = EvalCtx::new(&p, cid, &copy, Tick::ZERO);
        with_transform_memo(&p, || assert!(super::memo::key(&ctx, LayerId(1)).is_none()));
    }

    /// Nested comps (with parented layers) render the same frames with and without the memo.
    #[test]
    fn nested_comps_render_the_same_with_the_memo() {
        for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
            let mut p = Project::default();
            p.settings.bit_depth = depth;
            let outer = Comp::new(200, 100, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
            let mut inner = Comp::new(150, 100, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
            inner.layers = chain(&mut p, &inner).into();
            let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
            let mut layers = vec![];
            for (i, collapse) in [false, true].into_iter().enumerate() {
                let mut pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (150, 100), None);
                pre.props.prop_mut("transform/position").unwrap().value = Value::Vec3([60.0 + 80.0 * i as f64, 50.0, 0.0]);
                pre.switches.collapse = collapse;
                layers.push(pre);
            }
            let mut c = outer;
            c.layers = layers.into();
            let cid: ItemId = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(c.into()));
            for t in [0.0, 0.5] {
                let r = Renderer::new(&p, &NoFootage, RenderOpts::default());
                let a = r.comp_frame(cid, Tick::from_seconds_f64(t));
                NO_MEMO.with(|m| m.set(true));
                let b = r.comp_frame(cid, Tick::from_seconds_f64(t));
                NO_MEMO.with(|m| m.set(false));
                assert!(a.data.iter().any(|p| p[3] > 0.0));
                assert!(a == b, "{depth:?} t {t}: the memo changed the frame");
            }
        }
    }
}
