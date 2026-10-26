//! A bounded proof for completed RAM frames after one supported property edit.
//! Unknown temporal/dependency contexts keep the ordinary full-comp invalidation.
use super::{Cache, FrameKey, comp_content_with};
use effectcraft_engine::project::{AutoOrient, Comp, FrameBlend, GroupKind, ItemId, ItemKind, LayerSource, MaskMotionBlur, Node, Project, Property, Renderer};
use effectcraft_engine::render::EvalCtx;
use effectcraft_keyframe::Value;
use std::io::{self, Write};
use std::sync::Arc;

const MAX_LAYERS: usize = 256;
const MAX_NODES: usize = 8192;
const MAX_DEPTH: usize = 32;
const MAX_KEYS: usize = 256;
const MAX_BYTES: usize = 1 << 20;
const MAX_FRAMES: usize = 2048;

/// Bounded writer: validate clone size without allocating a serialized project.
struct SizeLimit(usize);
impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len()).ok_or_else(|| io::Error::other("property proof size limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_simple(comp: &Comp) -> bool {
    if comp.layers.len() > MAX_LAYERS || comp.renderer != Renderer::Classic3D {
        return false;
    }
    let mut remaining = MAX_NODES;
    for layer in &comp.layers {
        if !matches!(layer.source, LayerSource::Solid { .. } | LayerSource::Footage { .. } | LayerSource::Shape | LayerSource::Null)
            || layer.is_3d()
            || layer.switches.collapse
            || layer.switches.adjustment
            || layer.switches.motion_blur
            || layer.switches.frame_blend != FrameBlend::Off
            || layer.parent.is_some()
            || layer.track_matte.is_some()
            || layer.environment
            || layer.environment_background
            || !layer.stretch.is_finite()
            || layer.stretch == 0.0
            || layer.props.get("timeRemap").is_some()
            || layer.effects().is_some_and(|g| !g.children.is_empty())
        {
            return false;
        }
        // Iterators rather than a flattened tree: the stack is depth-bounded even for
        // a hostile group with millions of children.
        let mut stack = vec![layer.props.children.iter()];
        while let Some(iter) = stack.last_mut() {
            let Some(node) = iter.next() else {
                stack.pop();
                continue;
            };
            let Some(left) = remaining.checked_sub(1) else { return false };
            remaining = left;
            match node {
                Node::Prop(p) => {
                    if p.expr.is_some() || p.keys.len() > MAX_KEYS {
                        return false;
                    }
                }
                Node::Group(g) => {
                    if stack.len() >= MAX_DEPTH {
                        return false;
                    }
                    stack.push(g.children.iter());
                }
            }
        }
    }
    serde_json::to_writer(SizeLimit(MAX_BYTES), comp).is_ok()
}

const PATHS: [&str; 5] = ["transform/opacity", "transform/position", "transform/scale", "transform/anchor", "transform/rotation"];

#[derive(Clone, Copy)]
enum ValueKind {
    Scalar,
    Vec2,
    Vec3,
}

impl ValueKind {
    fn finite(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Scalar, Value::Scalar(v)) => v.is_finite(),
            (Self::Vec2, Value::Vec2(v)) => v.iter().all(|x| x.is_finite()),
            (Self::Vec3, Value::Vec3(v)) => v.iter().all(|x| x.is_finite()),
            _ => false,
        }
    }
    fn supported(self, p: &Property) -> bool {
        self.finite(&p.value) && p.keys.iter().all(|k| self.finite(&k.value))
    }
}

pub(super) struct Proof<'a> {
    before: &'a Project,
    after: &'a Project,
    old_comp: &'a Comp,
    new_comp: &'a Comp,
    layer: usize,
    path: String,
    kind: ValueKind,
}

impl<'a> Proof<'a> {
    pub(super) fn between(before: &'a Project, after: &'a Project, id: ItemId) -> Option<Self> {
        // Freeze previews can mask authoring edits and carry an independent verification
        // epoch. Their pixels cannot be proven from opacity properties alone.
        if [before, after].iter().any(|p| p.freeze.sections.iter().any(|s| s.enabled && s.target.comp() == id)) {
            return None;
        }
        let old_comp = before.comp(id)?;
        let new_comp = after.comp(id)?;
        if !bounded_simple(old_comp) || !bounded_simple(new_comp) || old_comp.layers.len() != new_comp.layers.len() {
            return None;
        }
        let mut changed = None;
        for (index, (old, new)) in old_comp.layers.iter().zip(new_comp.layers.iter()).enumerate() {
            if !std::ptr::eq(old, new) && old != new && changed.replace(index).is_some() {
                return None;
            }
        }
        let layer = changed?;
        let old_layer = old_comp.layers.get(layer)?;
        let new_layer = new_comp.layers.get(layer)?;
        let mut selected = None;
        for path in PATHS {
            let (Some(old), Some(new)) = (old_layer.props.prop(path), new_layer.props.prop(path)) else { continue };
            let kind = if matches!(path, "transform/position" | "transform/scale" | "transform/anchor") { ValueKind::Vec3 } else { ValueKind::Scalar };
            if old != new && selected.replace((path.to_owned(), kind)).is_some() {
                return None;
            }
        }
        // Direct mask properties only. Index-qualified paths distinguish masks even
        // in malformed trees with repeated names or uids; normalization below proves
        // that the entire remaining tree (including mask interpretation) is unchanged.
        if let Some(masks) = old_layer.masks() {
            let root = old_layer.props.children.iter().position(|n| n.as_group().is_some_and(|g| std::ptr::eq(g, masks)))?.checked_add(1)?;
            for (index, node) in masks.children.iter().enumerate() {
                let Node::Group(mask) = node else { continue };
                let GroupKind::Mask { motion_blur, .. } = mask.kind else { continue };
                let ordinal = index.checked_add(1)?;
                for (name, kind) in [("opacity", ValueKind::Scalar), ("feather", ValueKind::Vec2), ("expansion", ValueKind::Scalar)] {
                    let Some(property) = mask.children.iter().position(|n| n.as_prop().is_some_and(|p| p.match_id == name)) else { continue };
                    let property = property.checked_add(1)?;
                    let path = format!("#{root}/#{ordinal}/#{property}");
                    let (Some(old), Some(new)) = (old_layer.props.prop(&path), new_layer.props.prop(&path)) else { continue };
                    if old == new {
                        continue;
                    }
                    // Forced mask blur samples feather/expansion at shutter times
                    // when the path is animated, independently of the layer switch.
                    // Reject it even when currently inactive: frame options can vary.
                    // Opacity is applied after coverage, at the main context time.
                    if name != "opacity" && motion_blur == MaskMotionBlur::On {
                        return None;
                    }
                    if selected.replace((path, kind)).is_some() {
                        return None;
                    }
                }
            }
        }
        let (path, kind) = selected?;
        let old = old_layer.props.prop(&path)?;
        let new = new_layer.props.prop(&path)?;
        if !kind.supported(old) || !kind.supported(new) {
            return None;
        }
        if path.starts_with("transform/") && path != "transform/opacity" {
            // Along-path orientation samples neighboring times. Separated position
            // and compound edits need a different proof; keep full invalidation.
            let tr = old_layer.transform()?;
            if old_layer.auto_orient != AutoOrient::Off || ["positionX", "positionY", "positionZ"].iter().any(|p| tr.get(p).is_some()) {
                return None;
            }
        }
        // Only values/keyframes may change; metadata, expression and interpretation may not.
        let mut prop = new.clone();
        prop.value = old.value.clone();
        prop.keys.clone_from(&old.keys);
        if &prop != old {
            return None;
        }
        let mut normalized = new_comp.clone();
        *normalized.layers.get_mut(layer)?.props.prop_mut(&path)? = old.clone();
        if &normalized != old_comp {
            return None;
        }
        // Preserve the existing complete dependency/settings identity, substituting only
        // the comp for which the exact single-property comparison above succeeded.
        let old_id = super::comp_content(before, id);
        if comp_content_with(after, id, |cid, c| {
            if cid == id
                && let Some(ItemKind::Comp(old)) = before.item(cid).map(|i| &i.kind)
            {
                Arc::as_ptr(old)
            } else {
                Arc::as_ptr(c)
            }
        }) != old_id
        {
            return None;
        }
        Some(Self { before, after, old_comp, new_comp, layer, path, kind })
    }

    fn unchanged(&self, comp: ItemId, frame: i64) -> bool {
        let Some(old) = self.old_comp.layers.get(self.layer) else { return false };
        let Some(new) = self.new_comp.layers.get(self.layer) else { return false };
        let time = self.old_comp.frame_rate.tick_of(frame);
        // The renderer owns start/stretch mapping; no guessed keyframe influence interval.
        let a = EvalCtx::new(self.before, comp, self.old_comp, time);
        let b = EvalCtx::new(self.after, comp, self.new_comp, time);
        if self.path == "transform/opacity" {
            let (a, b) = (a.opacity(old), b.opacity(new));
            return a.is_finite() && b.is_finite() && a == b;
        }
        let (Some(old_prop), Some(new_prop)) = (old.props.prop(&self.path), new.props.prop(&self.path)) else { return false };
        let (a, b) = (a.value(old, old_prop), b.value(new, new_prop));
        self.kind.finite(&a) && self.kind.finite(&b) && a == b
    }

    pub(super) fn rekey(&self, cache: &mut Cache, old: u64, new: u64, revision: u64, comp: ItemId) {
        // Bound work, including scanning. Partial retention is always safe.
        let keys: Vec<_> =
            cache.order.iter().rev().take(MAX_FRAMES).filter(|k| k.content == old && k.comp == comp.0 && self.unchanged(comp, k.frame)).copied().collect();
        for previous in keys {
            let current = FrameKey { content: new, revision, ..previous };
            if cache.map.contains_key(&current) {
                continue;
            }
            if let Some(image) = cache.map.remove(&previous) {
                cache.map.insert(current, image);
                cache.move_draft(&previous, current);
                cache.moved(&previous, current);
                cache.order.rename(&previous, current);
                if let Some(rank) = cache.protected.remove(&previous) {
                    cache.protected.insert(current, rank);
                }
                cache.prints.rename(&previous, current);
                cache.generation = cache.generation.wrapping_add(1);
            }
        }
        // Ownership moved, so CPU/GPU byte accounting and LRU positions are unchanged.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{FrameImage, Frames};
    use effectcraft_engine::{Session, time::Tick};
    use effectcraft_keyframe::Keyframe;
    use serde_json::json;

    fn scene() -> (Arc<Project>, ItemId) {
        let mut s = Session::default();
        let id = ItemId(
            s.execute("comp.new", json!({"name":"Opacity retention", "width":64, "height":64, "duration":4, "frameRate":30})).unwrap()["comp"]
                .as_u64()
                .unwrap(),
        );
        s.execute("layer.newSolid", json!({"comp":id.0,"color":"#e02060"})).unwrap();
        let p = Arc::make_mut(&mut s.project);
        let opacity = p.comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut("transform/opacity").unwrap();
        opacity.keys = [0., 1., 2.].into_iter().map(|time| Keyframe::new(Tick::from_seconds_f64(time), Value::Scalar(100.))).collect();
        (s.project, id)
    }
    fn edit(before: &Arc<Project>, id: ItemId, key: usize) -> Arc<Project> {
        let mut after = before.clone();
        Arc::make_mut(&mut after)
            .comp_mut(id)
            .unwrap()
            .layers
            .get_mut(0)
            .unwrap()
            .props
            .prop_mut("transform/opacity")
            .unwrap()
            .keys
            .get_mut(key)
            .unwrap()
            .value = Value::Scalar(40.);
        after
    }
    fn key(content: u64, revision: u64, comp: ItemId, frame: i64) -> FrameKey {
        FrameKey { content, revision, comp: comp.0, frame, scale: 1000, view: 7, opts: 11 }
    }
    fn fill(f: &Frames, key: FrameKey) {
        f.cache.lock().unwrap().insert(key, FrameImage::Cpu(Arc::new(egui::ColorImage::new([8, 8], vec![egui::Color32::RED; 64]))));
    }

    #[test]
    fn opacity_reuse_preserves_exact_keys_budget_and_undo_redo() {
        let (before, id) = scene();
        let after = edit(&before, id, 1);
        let frames = Frames::default();
        let old = frames.content_of_preview(&before, 1, 101, id);
        let late = key(old, 1, id, 90);
        let inside = key(old, 1, id, 30);
        fill(&frames, late);
        fill(&frames, inside);
        frames.cache.lock().unwrap().protected.insert(late, 0);
        let bytes = frames.cache.lock().unwrap().bytes;
        let new = frames.content_of_preview(&after, 2, 202, id);
        assert_ne!(old, new);
        let moved = FrameKey { content: new, revision: 2, ..late };
        assert!(frames.is_cached(&moved));
        assert!(!frames.is_cached(&FrameKey { content: new, ..inside }));
        assert!(!frames.is_cached(&FrameKey { scale: 500, ..moved }));
        assert!(!frames.is_cached(&FrameKey { view: 8, ..moved }));
        assert!(!frames.is_cached(&FrameKey { opts: 12, ..moved }));
        {
            let cache = frames.cache.lock().unwrap();
            assert_eq!(cache.bytes, bytes);
            assert_eq!(cache.order.len(), 2);
            assert_eq!(cache.protected.get(&moved), Some(&0));
            assert_eq!(cache.map.get_key_value(&moved).unwrap().0.revision, 2);
        }
        let undo = frames.content_of(&before, 3, id);
        assert_eq!(undo, old);
        assert!(frames.is_cached(&FrameKey { content: undo, revision: 3, ..late }));
        assert!(frames.is_cached(&inside));
        let redo = frames.content_of(&after, 4, id);
        assert_eq!(redo, new);
        assert!(frames.is_cached(&FrameKey { content: redo, revision: 4, ..late }));
        assert_eq!(frames.cache.lock().unwrap().bytes, bytes);
        drop(before);
        drop(after);
        assert_eq!(frames.keepers.lock().unwrap().1.len(), 2);
    }

    #[test]
    fn active_freeze_sections_require_their_own_pixel_proof() {
        let (mut before, id) = scene();
        Arc::make_mut(&mut before).freeze.sections.push(effectcraft_engine::project::freeze::Section {
            id: 99,
            name: "Frozen scene".into(),
            target: effectcraft_engine::project::freeze::Target::Comp { comp: id },
            start: Tick::ZERO,
            end: Tick::from_seconds_f64(4.0),
            follow_comp_range: true,
            enabled: true,
            auto: false,
            quick_draft: false,
            cache: None,
            previous: vec![],
        });
        let after = edit(&before, id, 1);
        assert!(Proof::between(&before, &after, id).is_none());
        Arc::make_mut(&mut before).freeze.sections[0].enabled = false;
        let after = edit(&before, id, 1);
        assert!(Proof::between(&before, &after, id).is_some(), "live originals keep ordinary opacity-frame retention");
    }

    #[test]
    fn opacity_reuse_endpoint_holds_and_layer_time_match_renderer() {
        let (before, id) = scene();
        let after = edit(&before, id, 2);
        let proof = Proof::between(&before, &after, id).unwrap();
        assert!(!proof.unchanged(id, 90), "editing final key changes its held tail");
        assert!(proof.unchanged(id, 0));
        let mut shifted = before.clone();
        let l = Arc::make_mut(&mut shifted).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
        l.start_time = Tick::from_seconds_f64(1.);
        l.stretch = 200.;
        let changed = edit(&shifted, id, 1);
        let proof = Proof::between(&shifted, &changed, id).unwrap();
        assert!(proof.unchanged(id, 30));
        assert!(!proof.unchanged(id, 90), "three seconds maps to changed one-second layer key");
    }

    #[test]
    fn opacity_reuse_declines_unknown_dependencies_and_oversized_snapshots() {
        let (before, id) = scene();
        for mode in 0..11 {
            let mut old = before.clone();
            let c = Arc::make_mut(&mut old).comp_mut(id).unwrap();
            let l = c.layers.get_mut(0).unwrap();
            match mode {
                0 => l.props.prop_mut("transform/opacity").unwrap().expr = Some(Default::default()),
                1 => l.switches.motion_blur = true,
                2 => l.switches.frame_blend = FrameBlend::FrameMix,
                3 => l.source = LayerSource::Comp { item: id },
                4 => l.switches.adjustment = true,
                5 => l.props.prop_mut("transform/opacity").unwrap().keys.resize(MAX_KEYS + 1, Keyframe::new(Tick::ZERO, Value::Scalar(100.))),
                6 => l.comment = "x".repeat(MAX_BYTES + 1),
                7 => l.switches.three_d = true,
                8 => l.source = LayerSource::Text,
                9 => {
                    let mut p = l.props.prop("transform/opacity").unwrap().clone();
                    p.match_id = "timeRemap".into();
                    l.props.children.push(Node::Prop(p));
                }
                _ => {
                    let p = l.props.prop("transform/opacity").unwrap().clone();
                    l.props.sub_mut("effects").unwrap().children.push(Node::Prop(p));
                }
            }
            let changed = edit(&old, id, 1);
            assert!(Proof::between(&old, &changed, id).is_none(), "mode {mode}");
        }
        let mut changed = edit(&before, id, 1);
        Arc::make_mut(&mut changed).comp_mut(id).unwrap().width += 1;
        assert!(Proof::between(&before, &changed, id).is_none());
        let mut changed = edit(&before, id, 1);
        Arc::make_mut(&mut changed).settings.bit_depth = effectcraft_engine::project::BitDepth::Bpc32;
        assert!(Proof::between(&before, &changed, id).is_none());
    }

    #[test]
    fn opacity_reuse_agrees_with_actual_cpu_pixels() {
        let (before, id) = scene();
        let after = edit(&before, id, 1);
        let proof = Proof::between(&before, &after, id).unwrap();
        let fps = before.comp(id).unwrap().frame_rate;
        for frame in [0, 15, 30, 45, 60, 90] {
            let a = effectcraft_engine::render::render_frame(&before, id, fps.tick_of(frame), 1.);
            let b = effectcraft_engine::render::render_frame(&after, id, fps.tick_of(frame), 1.);
            assert_eq!(a.data == b.data, proof.unchanged(id, frame), "frame {frame}");
        }
    }

    #[test]
    #[ignore = "local serial CPU timing; coordinate with GPU performance runs"]
    fn opacity_reuse_cached_sequence_benchmark() {
        use std::time::Instant;
        let (mut before, id) = scene();
        let comp = Arc::make_mut(&mut before).comp_mut(id).unwrap();
        comp.width = 160;
        comp.height = 90;
        let mut layer = comp.layers.get(0).unwrap().clone();
        layer.props.prop_mut("transform/opacity").unwrap().keys.clear();
        for index in 1..80 {
            layer.id = effectcraft_engine::project::LayerId(1000 + index);
            comp.layers.push(layer.clone());
        }
        let after = edit(&before, id, 1);
        let fps = before.comp(id).unwrap().frame_rate;
        let mut timings = Vec::new();
        for enabled in [false, true, true, false] {
            let frames = Frames::default();
            let old = frames.content_of(&before, 1, id);
            // Full/Half and view variants, 2048 real cache entries, all in retained tail.
            for n in 0..MAX_FRAMES {
                let mut k = key(old, 1, id, 60 + (n / 4) as i64);
                k.scale = if n % 2 == 0 { 1000 } else { 500 };
                k.view = (n % 4 / 2) as u64;
                fill(&frames, k);
            }
            let start = Instant::now();
            let content = if enabled { frames.content_of(&after, 2, id) } else { super::super::comp_content(&after, id) };
            let proof_ms = start.elapsed().as_secs_f64() * 1000.;
            assert_eq!(frames.cache.lock().unwrap().map.keys().filter(|k| k.content == content).count(), if enabled { MAX_FRAMES } else { 0 });
            let start = Instant::now();
            let mut renders = 0;
            for frame in 0..120 {
                let k = FrameKey { view: 0, ..key(content, 2, id, frame) };
                if !frames.is_cached(&k) {
                    std::hint::black_box(effectcraft_engine::render::render_frame(&after, id, fps.tick_of(frame), 1.));
                    renders += 1;
                }
            }
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000. + proof_ms;
            timings.push((enabled, proof_ms, elapsed_ms, renders));
        }
        eprintln!("RT5_OPACITY_ABBA {timings:?}");
        assert!(timings.get(1).unwrap().3 < timings.first().unwrap().3);
    }
}

#[cfg(test)]
#[path = "property_reuse_tests.rs"]
mod transform_tests;
