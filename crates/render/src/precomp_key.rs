//! Layer cache keys of precomp layers: the nested comp's content at the nested time, so a
//! precomp whose nested comp doesn't change renders once (and an Essential Properties instance
//! doesn't copy the project on a hit).
//!
//! A nested comp's key ([`Renderer::comp_key`]) hashes everything its frame is made of: the
//! comp's settings and every layer (structure memoised per shared comp and layer, see
//! [`crate::cache`]), each animated property's value at the nested time (expressions included,
//! so one reading another comp is keyed by what it reads), which layers are active, the items
//! they show (footage by source time unless it is a still), nested precomps recursively, and the
//! renderer state that reaches nested comps (scale, quality, switches, proxies, depth).
//!
//! Each layer's structure counts by what reads it ([`Mode`]): a layer drawn from its values at
//! the frame time alone (`pointwise`, not motion blurred) without its keyframes, as the animated
//! values at the frame time decide, so a keyframe edit changes the key only where it changes a
//! value; a motion-blurred layer (and its parents, and the camera and lights of a blurred 3D
//! layer) with the keyframes its shutter's times read; a layer that reads other times (time
//! effects, Wiggle, paint spans, Auto-Orient along a path, frame blending, time remapping), and
//! every layer of a comp sampled at other times from above or by a layer that reads other layers
//! at other times, with every keyframe. The time counts when a layer is read at other times, and
//! such a layer's expressions must read nothing but their own value and the time
//! ([`crate::cache::free::self_contained`]): one that may read other layers or comps at those
//! times, out of the key's sight, leaves the comp unkeyed. A layer the frame doesn't use (not
//! active, and nothing used points at it) counts only by its place, expressions included.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use effectcraft_project::essential::{self, EgKind, Override};
use effectcraft_project::{AutoOrient, Comp, FootageKind, FrameBlend, ItemId, ItemKind, Layer, LayerId, LayerSource, Layers, Project};
use effectcraft_time::Tick;

use crate::cache::{FootageKey, LayerInfo, Memo, ProxyKey, hash_debug, hash_values, hasher, layer_info_arc};
use crate::footage::frame_blend_mode;
use crate::sources::essential_overrides;
use crate::{EvalCtx, RenderOpts, Renderer};

/// Nesting limit for keys (as for rendering, see `Renderer::comp_frame_cpu`).
const MAX_DEPTH: usize = 16;

/// What a comp's key needs that changes only when the comp does (memoised per shared comp).
struct CompInfo {
    /// The comp's settings, hashed.
    settings: u64,
    /// Every layer's structure (see [`LayerInfo`]), in order.
    layers: Vec<LayerInfo>,
    /// Each layer's index by id (for parent chains).
    index: HashMap<LayerId, usize>,
    /// A layer that reads other times also reads other layers (a layer parameter, which effects
    /// may read at other times, an effect reading the comp's scene, paint's Clone strokes): every
    /// layer is read at other times. (An adjustment layer's effects read the layers below at the
    /// frame time alone: `EffectHost::self_at` has nothing for them.)
    spread: bool,
    /// Layers with keyframes or expressions: their values are hashed per frame.
    animated: Vec<usize>,
    /// Layers showing an item (footage, solid, precomp, model).
    sources: Vec<usize>,
    /// Nested comps (precomp layers' items).
    nested: Vec<ItemId>,
    /// A property has an expression.
    expressions: bool,
    /// A layer reads the clock or other times by itself (time effects, Wiggle, paint, Auto-Orient
    /// along a path).
    other_times: bool,
}

impl CompInfo {
    fn of(c: &Comp) -> CompInfo {
        let mut h = hasher(0x51ed_270b_27d3_8d41);
        let mut settings = c.clone();
        settings.layers = Layers::new();
        hash_debug(&mut h, &settings);
        let mut info = CompInfo {
            settings: h.finish(),
            layers: vec![],
            index: HashMap::new(),
            spread: false,
            animated: vec![],
            sources: vec![],
            nested: vec![],
            expressions: false,
            other_times: false,
        };
        for (i, l) in c.layers.arcs().iter().enumerate() {
            let li = layer_info_arc(l);
            info.layers.push(li);
            info.index.insert(l.id, i);
            info.spread |= !li.pointwise && (li.layer_refs || li.reads_comp_scene || li.paint);
            if li.animated {
                info.animated.push(i);
            }
            if l.source.item().is_some() {
                info.sources.push(i);
            }
            if let LayerSource::Comp { item } = l.source {
                info.nested.push(item);
            }
            info.expressions |= li.expressions;
            info.other_times |= li.time_dependent || li.reads_other_times || li.paint || l.auto_orient == AutoOrient::AlongPath;
        }
        info
    }
}

static COMPS: LazyLock<RwLock<Memo<Comp, Arc<CompInfo>>>> = LazyLock::new(|| RwLock::new(Memo::new()));

/// Comp and layer pairs already reported by [`warn_unkeyed`].
static UNKEYED: LazyLock<Mutex<HashSet<(ItemId, LayerId)>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Say once per comp and layer that `layer`'s expressions keep comp `comp` out of the layer
/// cache (they may read other layers or comps out of the key's sight), so every precomp of it
/// is drawn again on every frame. Removing those expressions, or baking them, makes it cacheable.
fn warn_unkeyed(project: &Project, comp: ItemId, layer: &Layer) {
    let Ok(mut seen) = UNKEYED.lock() else { return };
    if seen.len() > 4096 {
        seen.clear();
    }
    if seen.insert((comp, layer.id)) {
        let name = project.item(comp).map_or("?", |i| i.name.as_str());
        log::warn!(
            target: "effectcraft_cache",
            "comp {name:?} can't be cached: the expressions of its layer {:?} may read other layers or comps, so it is drawn again on every frame",
            layer.name
        );
    }
}

fn comp_info(arc: &Arc<Comp>) -> Arc<CompInfo> {
    if let Some(i) = COMPS.read().ok().and_then(|m| m.get(Arc::as_ptr(arc))) {
        return i;
    }
    let info = Arc::new(CompInfo::of(arc));
    if let Ok(mut m) = COMPS.write() {
        m.put(arc, info.clone());
    }
    info
}

/// The shared handle of comp `id`.
fn comp_arc(project: &Project, id: ItemId) -> Option<&Arc<Comp>> {
    match &project.item(id)?.kind {
        ItemKind::Comp(c) => Some(c),
        _ => None,
    }
}

/// Does comp `id` or a comp nested in it have an expression?
fn tree_has_expressions(project: &Project, id: ItemId, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return true;
    }
    let Some(arc) = comp_arc(project, id) else { return false };
    let info = comp_info(arc);
    info.expressions || info.nested.iter().any(|n| tree_has_expressions(project, *n, depth + 1))
}

/// Does comp `id` change from frame to frame: a layer animates or reads the clock in it or a
/// comp nested in it, or it shows moving footage?
fn tree_varies(project: &Project, id: ItemId, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return true;
    }
    let Some(arc) = comp_arc(project, id) else { return false };
    let info = comp_info(arc);
    if !info.animated.is_empty() || info.other_times {
        return true;
    }
    let moving = |l: &Layer| match l.source {
        LayerSource::Footage { item } => !matches!(project.item(item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if f.kind == FootageKind::Still),
        _ => false,
    };
    info.sources.iter().any(|&i| arc.layers.get(i).is_some_and(moving)) || info.nested.iter().any(|n| tree_varies(project, *n, depth + 1))
}

impl<'a> Renderer<'a> {
    /// Whether precomp layer `layer`'s pixels change from frame to frame (its nested comps
    /// animate, or its own masks or effects do): its buffer is only reused within a frame (see
    /// [`crate::cache::LayerCache::insert_once`]).
    pub(crate) fn precomp_varies(&self, ctx: &EvalCtx, layer: &Layer) -> bool {
        let LayerSource::Comp { item } = layer.source else { return false };
        crate::cache::content_animated(ctx.comp, layer) || tree_varies(self.project, item, 0)
    }

    /// The key of what precomp layer `layer` shows at the context time (its nested comp as
    /// [`Self::source`] draws it), or `None` when it can't be keyed (see the module docs).
    pub(crate) fn precomp_key(&self, ctx: &EvalCtx, layer: &Layer) -> Option<u64> {
        self.precomp_key_in(ctx, layer, true)
    }

    /// [`Self::precomp_key`]; `free`: nothing above samples the nested comp at other times, so
    /// its key may leave keyframes out (see the module docs).
    fn precomp_key_in(&self, ctx: &EvalCtx, layer: &Layer, free: bool) -> Option<u64> {
        let LayerSource::Comp { item } = layer.source else { return None };
        // The layer itself samples its nested comp at other times (time effects, frame
        // blending, time remapping), or draws its layers at the shutter's times (collapsed with
        // motion blur; a precomp that isn't collapsed is drawn once and its buffer moved).
        let collapsed_blur = layer.switches.collapse && self.opts.motion_blur && ctx.comp.enable_motion_blur;
        let free = free && crate::cache::layer_info(ctx.comp, layer).pointwise && !collapsed_blur;
        let mut h = hasher(0x2545_f491_4f6c_dd1d);
        // Nothing is drawn (the comp contains this one, or the time is outside it).
        if self.project.comp_contains(item, ctx.comp_id) {
            0u8.hash(&mut h);
            return Some(h.finish());
        }
        let lt = ctx.nested_time(layer);
        let nc = self.project.comp(item)?;
        if !nc.covers(lt) {
            1u8.hash(&mut h);
            return Some(h.finish());
        }
        if let Some(pf) = self.proxy_for(item) {
            2u8.hash(&mut h);
            hash_debug(&mut h, &FootageKey(pf));
            lt.0.hash(&mut h);
            return Some(h.finish());
        }
        // Frame blending mixes two nested frames: not keyed.
        if frame_blend_mode(ctx, layer) != FrameBlend::Off {
            return None;
        }
        let (sub, bs) = self.precomp_renderer(layer, item);
        3u8.hash(&mut h);
        bs.to_bits().hash(&mut h);
        let ov = essential_overrides(ctx, layer);
        let key = if ov.is_empty() {
            sub.comp_key_in(item, lt, free)?
        } else {
            hash_debug(&mut h, &ov);
            if plain_overrides(self.project, item, nc, &ov) {
                // The instance's values are in `ov`: no copy of the project.
                sub.comp_key_in(item, lt, free)?
            } else {
                match essential::with_overrides(self.project, item, &ov) {
                    Some(p) => Renderer { project: &p, ..sub }.comp_key_in(item, lt, free)?,
                    None => sub.comp_key_in(item, lt, free)?,
                }
            }
        };
        key.hash(&mut h);
        Some(h.finish())
    }

    /// The content key of comp `id` drawn by this renderer at `t` (see the module docs):
    /// keyframes left out where that is safe.
    pub(crate) fn comp_key(&self, id: ItemId, t: Tick) -> Option<u64> {
        self.comp_key_in(id, t, true)
    }

    /// [`Self::comp_key`]; `free`: nothing above samples this comp at other times.
    pub(crate) fn comp_key_in(&self, id: ItemId, t: Tick, free: bool) -> Option<u64> {
        if self.depth > MAX_DEPTH {
            return None;
        }
        let arc = comp_arc(self.project, id)?;
        let info = comp_info(arc);
        let nc: &Comp = arc;
        let ctx = self.ctx(id, nc, t);
        let (modes, sampled) = self.modes(&ctx, &info, free);
        let strict = !free || info.spread;
        let mut h = hasher(0x9ae1_6a3b_2f90_404f);
        info.settings.hash(&mut h);
        // Only the layers the frame uses count by their content (see [`needed_layers`]); the
        // others by their place, so an edit to a layer that isn't on screen keeps the key.
        let needed = if strict { None } else { Some(needed_layers(&ctx, &info, t)) };
        let used = |i: usize| needed.as_ref().is_none_or(|n| n.get(i).copied().unwrap_or(true));
        // Layers read at other times than the frame's: their expressions must read nothing else.
        let mut timed = false;
        for (i, ((l, li), mode)) in nc.layers.iter().zip(&info.layers).zip(&modes).enumerate() {
            let lights = matches!(l.source, LayerSource::Camera | LayerSource::Light { .. });
            let contained = match mode {
                Mode::Free => true,
                _ if strict || lights => li.contained,
                Mode::Window(..) => li.contained_sampled,
                Mode::Whole => li.contained_inner && (!sampled.get(i).copied().unwrap_or(true) || li.contained_sampled),
            };
            if !used(i) {
                // Unused: its id and switches (solo hides the comp's other layers at every time).
                // Its expressions don't count either: the frame doesn't draw it, and a used layer
                // that reads it by an expression is keyed by the value it reads (or isn't keyed).
                (0xdead_u16, l.id).hash(&mut h);
                hash_debug(&mut h, &l.switches);
                continue;
            }
            if li.expressions && !contained {
                warn_unkeyed(self.project, id, l);
                return None;
            }
            timed |= *mode != Mode::Free;
            hash_debug(&mut h, mode);
            match mode {
                Mode::Free => li.free.hash(&mut h),
                Mode::Whole => li.whole.hash(&mut h),
                Mode::Window(t0, t1) => {
                    li.free.hash(&mut h);
                    let (a, b) = (l.layer_time(*t0), l.layer_time(*t1));
                    crate::cache::free::hash_window(&mut h, &l.props, a.min(b), a.max(b));
                }
            }
        }
        id.hash(&mut h);
        hash_debug(&mut h, &self.project.settings);
        hash_debug(&mut h, &RenderOpts { view: None, roi: None, ..self.opts });
        hash_debug(&mut h, &self.inherited);
        self.depth.hash(&mut h);
        self.active_accel().is_some().hash(&mut h);
        if timed {
            t.0.hash(&mut h);
        }
        for l in nc.layers.iter() {
            l.is_active_at(t).hash(&mut h);
        }
        for &i in info.animated.iter().filter(|i| used(**i)) {
            let l = nc.layers.get(i)?;
            hash_values(&mut h, &ctx, l, &l.props);
        }
        for &i in info.sources.iter().filter(|i| used(**i)) {
            let l = nc.layers.get(i)?;
            match &l.source {
                LayerSource::Footage { item } | LayerSource::Model { item } => {
                    let it = self.project.item(*item);
                    match it.map(|it| (&it.kind, &it.proxy)) {
                        Some((ItemKind::Footage(f), proxy)) => {
                            hash_debug(&mut h, &FootageKey(f));
                            hash_debug(&mut h, &ProxyKey(proxy));
                            let used = self.proxy_for(*item);
                            used.is_some().hash(&mut h);
                            if used.unwrap_or(f).kind != FootageKind::Still {
                                ctx.source_time(l).0.hash(&mut h);
                            }
                        }
                        other => hash_debug(&mut h, &other.map(|o| o.0)),
                    }
                }
                LayerSource::Solid { item } => hash_debug(&mut h, &self.project.item(*item).map(|i| &i.kind)),
                LayerSource::Comp { .. } => self.precomp_key_in(&ctx, l, modes.get(i).is_some_and(|m| *m != Mode::Whole))?.hash(&mut h),
                _ => {}
            }
        }
        Some(h.finish())
    }
}

/// The layers of `ctx`'s comp a frame at `t` can draw or read: the layers active at `t`, the
/// cameras and lights, and what those use, followed transitively: parents (a parent moves its
/// children outside its own span), track mattes and the layers effects read (layer parameters).
/// The renderer draws only layers active at the frame's time, so the others can't change it.
fn needed_layers(ctx: &EvalCtx, info: &CompInfo, t: Tick) -> Vec<bool> {
    let nc = ctx.comp;
    let n = nc.layers.len();
    let mut needed = vec![false; n];
    let mut todo: Vec<usize> = Vec::new();
    for (i, l) in nc.layers.iter().enumerate() {
        if l.is_active_at(t) || matches!(l.source, LayerSource::Camera | LayerSource::Light { .. }) || l.environment {
            todo.push(i);
        }
    }
    let mut refs: Vec<LayerId> = Vec::new();
    while let Some(i) = todo.pop() {
        let Some(slot) = needed.get_mut(i) else { continue };
        if *slot {
            continue;
        }
        *slot = true;
        let Some(l) = nc.layers.get(i) else { continue };
        refs.clear();
        refs.extend(l.parent);
        refs.extend(l.track_matte.as_ref().map(|m| m.layer));
        layer_params(ctx, l, &l.props, 0, &mut refs);
        for r in &refs {
            match info.index.get(r) {
                Some(j) => todo.push(*j),
                // A reference this comp can't resolve: count every layer.
                None => return vec![true; n],
            }
        }
    }
    needed
}

/// The layers `layer`'s layer parameters in `g` point at, at the context time.
fn layer_params(ctx: &EvalCtx, layer: &Layer, g: &effectcraft_project::PropGroup, depth: usize, out: &mut Vec<LayerId>) {
    use effectcraft_project::Node;
    if depth > 32 {
        return;
    }
    for node in &g.children {
        match node {
            Node::Prop(p) if matches!(p.value, effectcraft_keyframe::Value::Layer(_)) => {
                if let Some(id) = ctx.value(layer, p).as_layer() {
                    out.push(LayerId(id));
                }
            }
            Node::Group(sub) => layer_params(ctx, layer, sub, depth + 1, out),
            Node::Prop(_) => {}
        }
    }
}

/// How a layer's structure counts in its comp's key (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// Drawn from its values at the frame time alone: structure without keyframes.
    Free,
    /// Drawn at the times of a motion-blur shutter within these comp times: structure without
    /// keyframes, plus the keyframes those times read.
    Window(Tick, Tick),
    /// Read at other times: every keyframe.
    Whole,
}

impl Mode {
    /// The stricter of two modes.
    fn max(self, other: Mode) -> Mode {
        match (self, other) {
            (Mode::Whole, _) | (_, Mode::Whole) => Mode::Whole,
            (Mode::Window(a, b), Mode::Window(c, d)) => Mode::Window(a.min(c), b.max(d)),
            (Mode::Window(a, b), Mode::Free) | (Mode::Free, Mode::Window(a, b)) => Mode::Window(a, b),
            (Mode::Free, Mode::Free) => Mode::Free,
        }
    }
}

impl Renderer<'_> {
    /// Each layer's [`Mode`] in the comp `ctx` draws at its time, and whether motion blur reads
    /// its transform at the shutter's times. `free`: nothing above samples this comp at other
    /// times.
    fn modes(&self, ctx: &EvalCtx, info: &CompInfo, free: bool) -> (Vec<Mode>, Vec<bool>) {
        let nc = ctx.comp;
        let n = info.layers.len();
        if !free || info.spread {
            return (vec![Mode::Whole; n], vec![true; n]);
        }
        // The shutter's comp times, a frame wider on each side (see `motion_blur::Shutter`).
        let fd = nc.frame_duration().0 as f64;
        let (phase, angle) = (nc.shutter_phase / 360.0, nc.shutter_angle / 360.0);
        let (lo, hi) = (phase.min(phase + angle) - 1.0, phase.max(phase + angle) + 1.0);
        let at = |x: f64| Tick(ctx.time.0.saturating_add((x * fd) as i64));
        // Inside collapsed precomps the shutter is the outermost comp's, its times mapped down:
        // every keyframe counts.
        let collapsed = self.through.first().is_some_and(Option::is_some);
        let window = if collapsed || !lo.is_finite() || !hi.is_finite() { Mode::Whole } else { Mode::Window(at(lo), at(hi)) };
        let mut sampled: Vec<bool> = nc.layers.iter().map(|l| self.blurred(ctx, l)).collect();
        let mut modes: Vec<Mode> = info
            .layers
            .iter()
            .zip(&sampled)
            .map(|(li, blurred)| {
                if !li.pointwise {
                    Mode::Whole
                } else if *blurred {
                    window
                } else {
                    Mode::Free
                }
            })
            .collect();
        // A blurred 3D layer moves against the camera and lights at the shutter's times.
        if nc.layers.iter().zip(&sampled).any(|(l, b)| *b && l.switches.three_d) {
            for ((l, m), b) in nc.layers.iter().zip(modes.iter_mut()).zip(sampled.iter_mut()) {
                if matches!(l.source, LayerSource::Camera | LayerSource::Light { .. }) {
                    *m = m.max(window);
                    *b = true;
                }
            }
        }
        // Parents' transforms are read at their sampled children's times.
        for i in 0..n {
            if !sampled.get(i).copied().unwrap_or(false) {
                continue;
            }
            let mut parent = nc.layers.get(i).and_then(|l| l.parent);
            for _ in 0..n {
                let Some(j) = parent.and_then(|p| info.index.get(&p).copied()) else { break };
                if let Some(m) = modes.get_mut(j) {
                    *m = m.max(window);
                }
                if let Some(b) = sampled.get_mut(j) {
                    *b = true;
                }
                parent = nc.layers.get(j).and_then(|l| l.parent);
            }
        }
        (modes, sampled)
    }
}

/// Instance overrides that the original project plus the override values determine: no Media
/// replacement (another item) and no expression in the nested comps (one could read an
/// overridden property).
fn plain_overrides(project: &Project, item: ItemId, nc: &Comp, ov: &[Override]) -> bool {
    let media = nc.essential.as_ref().is_some_and(|eg| ov.iter().any(|o| eg.resolve(o.control).is_some_and(|c| matches!(c.kind, EgKind::Media { .. }))));
    !media && !tree_has_expressions(project, item, 0)
}
