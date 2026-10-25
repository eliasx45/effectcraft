//! Smart Freeze snapshot preparation. Expensive dependency capture belongs on a worker;
//! attachment checks use the immutable item handles captured when the work began.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use effectcraft_project::freeze::{Section, Target};
use effectcraft_project::{ItemId, ItemKind, LayerId, LayerSource, Node, Project};

const MAX_ITEMS: usize = 10_000;
const MAX_NODES: usize = 100_000;

/// Dependencies plus a render-only snapshot. `original` is also the source of live audio.
pub struct Capture {
    pub original: Arc<Project>,
    pub render: Arc<Project>,
    pub items: BTreeSet<ItemId>,
    pub identity: String,
    pub media: Vec<MediaStamp>,
    pub target: Target,
    pub layer_dependencies: Option<BTreeSet<LayerId>>,
    pub font_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MediaStamp {
    pub path: String,
    pub len: u64,
    pub modified_ns: u128,
    #[cfg(unix)]
    pub inode: u64,
    #[cfg(unix)]
    pub changed: (i64, i64),
}

pub(crate) fn media_stamp(path: &str) -> Result<MediaStamp, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("Smart Freeze cannot verify {path}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("Smart Freeze source is not a file: {path}"));
    }
    let modified_ns = metadata
        .modified()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other))
        .map_err(|e| format!("Smart Freeze cannot verify modification time for {path}: {e}"))?
        .as_nanos();
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(MediaStamp {
        path: path.into(),
        len: metadata.len(),
        modified_ns,
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

/// Whether any layer of `cid` (only the `only` layers when given) has an expression.
fn expressions(project: &Project, cid: ItemId, only: Option<&BTreeSet<LayerId>>) -> Result<bool, String> {
    let comp = project.comp(cid).ok_or("Smart Freeze composition is missing")?;
    let mut nodes: Vec<_> =
        comp.layers.iter().filter(|l| only.is_none_or(|ids| ids.contains(&l.id))).flat_map(|l| l.props.children.iter().map(|n| (n, 0usize))).collect();
    let mut count = 0usize;
    let mut found = false;
    while let Some((node, depth)) = nodes.pop() {
        count = count.saturating_add(1);
        if depth > 64 || count > MAX_NODES || nodes.len() > MAX_NODES {
            return Err("Smart Freeze property tree exceeds the verification budget".into());
        }
        match node {
            Node::Prop(p) => {
                count = count.saturating_add(p.keys.len());
                if count > MAX_NODES {
                    return Err("Smart Freeze keyframes exceed the verification budget".into());
                }
                found |= p.has_expression();
            }
            Node::Group(g) => nodes.extend(g.children.iter().map(|n| (n, depth.saturating_add(1)))),
        }
    }
    Ok(found)
}

fn expression_items(project: &Project, cid: ItemId) -> Result<Option<Vec<ItemId>>, String> {
    let comp = project.comp(cid).ok_or("Smart Freeze composition is missing")?;
    let mut refs = vec![];
    let mut opaque = false;
    let mut over_budget = false;
    for layer in &comp.layers {
        layer.props.walk("", &mut |_, p| {
            if p.has_expression()
                && let Some(expr) = &p.expr
            {
                match crate::freeze_expressions::references(&expr.text) {
                    Some(names) if refs.len().saturating_add(names.len()) <= MAX_ITEMS => refs.extend(names),
                    Some(_) => over_budget = true,
                    None => opaque = true,
                }
            }
        });
    }
    if over_budget {
        return Err("Smart Freeze expression references exceed their budget".into());
    }
    if opaque {
        return Ok(None);
    }
    let mut ids = BTreeSet::new();
    for (is_comp, name) in refs {
        let found: Vec<_> = project
            .items
            .values()
            .filter(|i| i.name == name && if is_comp { matches!(i.kind, ItemKind::Comp(_)) } else { matches!(i.kind, ItemKind::Footage(_)) })
            .map(|i| i.id)
            .collect();
        if found.is_empty() {
            return Err(format!("Smart Freeze cannot resolve expression source {name}"));
        }
        ids.extend(found);
        if ids.len() > MAX_ITEMS {
            return Err("Smart Freeze expression references exceed their budget".into());
        }
    }
    Ok(Some(ids.into_iter().collect()))
}

/// Resolve native layer links (including keyed Layer controls) and scene context without
/// depending on unrelated footage elsewhere in the parent composition.
fn selection_dependencies(project: &Project, target: &Target) -> Result<Option<BTreeSet<LayerId>>, String> {
    let Target::Selection { comp, layers } = target else { return Ok(None) };
    let comp = project.comp(*comp).ok_or("Smart Freeze composition is missing")?;
    let mut pending = layers.clone();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if seen.len() > MAX_ITEMS {
            return Err("Smart Freeze layer links exceed the dependency budget".into());
        }
        if !seen.insert(id) {
            continue;
        }
        let layer = comp.layer(id).ok_or_else(|| format!("Smart Freeze cannot resolve linked layer {}", id.0))?;
        if let Some(parent) = layer.parent {
            pending.push(parent);
        }
        if let Some(matte) = layer.track_matte {
            pending.push(matte.layer);
        }
        layer.props.walk("", &mut |_, p| {
            for value in std::iter::once(&p.value).chain(p.keys.iter().map(|k| &k.value)) {
                if let effectcraft_keyframe::Value::Layer(Some(id)) = value {
                    pending.push(LayerId(*id));
                }
            }
        });
        if layer.effects().is_some_and(|fx| {
            fx.groups().any(|g| {
                matches!(&g.kind,
            effectcraft_project::GroupKind::Effect {effect} if effectcraft_effects::READS_COMP_SCENE.contains(&effect.as_str()))
            })
        }) {
            pending.extend(comp.layers.iter().filter(|l| l.is_camera() || l.is_light()).map(|l| l.id));
        }
        if pending.len() > MAX_NODES {
            return Err("Smart Freeze layer links exceed the dependency budget".into());
        }
    }
    Ok(Some(seen))
}

fn same_comp_header(a: &effectcraft_project::Comp, b: &effectcraft_project::Comp) -> bool {
    let (mut a, mut b) = (a.clone(), b.clone());
    a.layers = Default::default();
    b.layers = Default::default();
    a == b
}

/// Validate context before hiding any layers in the render-only snapshot.
pub fn validate_target(project: &Project, target: &Target) -> Result<(), String> {
    let cid = target.comp();
    let comp = project.comp(cid).ok_or("Smart Freeze composition is missing")?;
    if comp.frame_rate.num <= 0 || comp.frame_rate.den <= 0 || comp.width == 0 || comp.height == 0 {
        return Err("Smart Freeze composition has invalid dimensions or frame rate".into());
    }
    if !matches!(target, Target::Selection { .. }) {
        for (_, parent) in project.comps() {
            for layer in &parent.layers {
                if matches!(layer.source, LayerSource::Comp { item } if item == cid)
                    && (layer.switches.collapse || layer.is_3d() || !effectcraft_project::essential::overridden(layer).is_empty())
                {
                    return Err(format!("{} uses this comp with collapse, 3D or Essential Properties; freeze the containing scene instead", layer.name));
                }
            }
        }
        return Ok(());
    }
    let Target::Selection { layers, .. } = target else { return Ok(()) };
    let selected: BTreeSet<_> = layers.iter().copied().collect();
    if selected.is_empty() || selected.len() != layers.len() {
        return Err("Smart Freeze needs a nonempty selection without duplicates".into());
    }
    let positions: Vec<_> = comp.layers.iter().enumerate().filter(|(_, l)| selected.contains(&l.id)).map(|(i, _)| i).collect();
    if positions.len() != selected.len() || positions.windows(2).any(|w| w.first().zip(w.get(1)).is_some_and(|(a, b)| a.checked_add(1) != Some(*b))) {
        return Err("Smart Freeze selection must be adjacent layers in one composition".into());
    }
    // Expressions elsewhere in the comp cannot change how the selection renders; only the
    // selected layers and the layers they link to matter.
    let linked = selection_dependencies(project, target)?.unwrap_or_default();
    if expressions(project, cid, Some(&linked))? {
        let names: Vec<_> = comp
            .layers
            .iter()
            .filter(|l| {
                let mut found = false;
                l.props.walk("", &mut |_, p| found |= p.has_expression());
                linked.contains(&l.id) && found
            })
            .map(|l| l.name.as_str())
            .collect();
        return Err(format!("{} has expressions; freeze the complete composition instead", names.join(", ")));
    }
    for layer in &comp.layers {
        if !selected.contains(&layer.id) {
            if layer.track_matte.is_some_and(|m| selected.contains(&m.layer)) {
                return Err(format!("{} uses the selected layers as a matte; freeze the complete composition", layer.name));
            }
            continue;
        }
        let reason = if let Some(parent) = layer.parent.filter(|id| !selected.contains(id)) {
            let parent = comp.layer(parent).map_or("a layer", |p| p.name.as_str());
            Some(format!("is parented to {parent}, which is not selected"))
        } else if layer.is_3d() {
            Some("is a 3D layer".into())
        } else if layer.switches.locked {
            Some("is locked".into())
        } else if layer.switches.guide {
            Some("is a guide layer".into())
        } else if layer.switches.adjustment {
            Some("is an adjustment layer".into())
        } else if layer.switches.collapse {
            Some("collapses transformations".into())
        } else if layer.track_matte.is_some() {
            Some("uses a track matte".into())
        } else if layer.preserve_transparency {
            Some("preserves underlying transparency".into())
        } else if layer.blend_mode != effectcraft_color::BlendMode::Normal {
            Some("uses a blending mode".into())
        } else if matches!(layer.source, LayerSource::Null) {
            Some("is a null".into())
        } else if layer.props.sub(effectcraft_project::styles::GROUP).is_some_and(|g| !g.children.is_empty()) {
            Some("has layer styles".into())
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(format!("{} {reason}; freeze the complete composition instead", layer.name));
        }
    }
    Ok(())
}

impl Capture {
    /// Call off the UI thread: traverses dependencies, serializes identity and stats media.
    pub fn new(original: Arc<Project>, section: &Section) -> Result<Self, String> {
        section.validate()?;
        validate_target(&original, &section.target)?;
        let (start, end) = section.range(original.comp(section.target.comp()).ok_or("Smart Freeze composition is missing")?.duration)?;
        let mut layer_dependencies = selection_dependencies(&original, &section.target)?;
        let mut items = BTreeSet::new();
        let mut pending = vec![(section.target.comp(), 0usize)];
        while let Some((id, depth)) = pending.pop() {
            if depth > 32 || items.len() >= MAX_ITEMS {
                return Err("Smart Freeze dependencies exceed the nesting/item budget".into());
            }
            if !items.insert(id) {
                continue;
            }
            let item = original.item(id).ok_or("Smart Freeze dependency is missing")?;
            if let ItemKind::Comp(comp) = &item.kind {
                // In the selection's own comp only the linked layers can affect the frames.
                let only = if id == section.target.comp() { layer_dependencies.as_ref() } else { None };
                if expressions(&original, id, only)? {
                    // A nested expression can inspect its parent comp. Keep that context in
                    // full, even when this freeze targets a selection in the parent.
                    if layer_dependencies.take().is_some() {
                        items.remove(&section.target.comp());
                        pending.push((section.target.comp(), 0));
                    }
                    match expression_items(&original, id)? {
                        Some(ids) => pending.extend(ids.into_iter().map(|id| (id, depth.saturating_add(1)))),
                        None => return Err("Smart Freeze cannot verify dynamic or external expression dependencies; originals remain live".into()),
                    }
                }
                for layer in &comp.layers {
                    if id == section.target.comp()
                        && let Some(layers) = &layer_dependencies
                        && !layers.contains(&layer.id)
                    {
                        continue;
                    }
                    if let LayerSource::Comp { item } = layer.source
                        && original.comp_contains(item, id)
                    {
                        return Err(format!("Smart Freeze found a precomposition cycle at {}; remove the circular reference before freezing", layer.name));
                    }
                    if let Some(source) = layer.source.item() {
                        pending.push((source, depth.saturating_add(1)));
                    }
                }
            }
        }
        let mut paths = BTreeSet::new();
        for id in &items {
            if original.comp(*id).is_some() {
                expressions(&original, *id, None)?;
            }
            if let Some(item) = original.item(*id) {
                let mut sources = vec![];
                if let ItemKind::Footage(f) = &item.kind {
                    sources.push(f);
                }
                if let Some(proxy) = &item.proxy {
                    sources.push(&proxy.footage);
                }
                for f in sources {
                    if !f.path.is_empty() {
                        paths.insert(f.path.clone());
                    }
                    paths.extend(f.sequence.iter().cloned());
                    if paths.len() > effectcraft_project::freeze::MAX_FRAMES as usize {
                        return Err("Smart Freeze media sequence exceeds its verification budget".into());
                    }
                }
            }
        }
        let (font_signatures, font_generation) =
            crate::freeze_fonts::dependencies(&original, &items, section.target.comp(), layer_dependencies.as_ref(), &mut paths)?;
        let media = crate::freeze_media::stamps(paths.into_iter().collect())?;
        let render = original.clone();
        let deps: Vec<std::borrow::Cow<'_, effectcraft_project::Item>> = items
            .iter()
            .filter_map(|id| {
                original.item(*id).map(|item| {
                    if *id == section.target.comp()
                        && let Some(ids) = &layer_dependencies
                    {
                        let mut scoped = item.clone();
                        if let ItemKind::Comp(comp) = &mut scoped.kind {
                            Arc::make_mut(comp).layers.retain(|l| ids.contains(&l.id));
                        }
                        std::borrow::Cow::Owned(scoped)
                    } else {
                        std::borrow::Cow::Borrowed(item)
                    }
                })
            })
            .collect();
        let mut hash = effectcraft_render::disk_cache::Hash128::default();
        struct HashWriter<'a> {
            hash: &'a mut effectcraft_render::disk_cache::Hash128,
            bytes: usize,
        }
        impl std::io::Write for HashWriter<'_> {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.bytes = self.bytes.saturating_add(data.len());
                if self.bytes > 128 * 1024 * 1024 {
                    return Err(std::io::Error::other("Smart Freeze identity exceeds its verification budget"));
                }
                self.hash.write(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(HashWriter { hash: &mut hash, bytes: 0 }, &(&original.settings, &section.target, start, end, deps, &media, font_signatures))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            original,
            render,
            items,
            identity: format!("{:032x}", hash.finish()),
            media,
            target: section.target.clone(),
            layer_dependencies,
            font_generation,
        })
    }

    /// Cheap conservative attachment check. Unrelated comps may change while this renders.
    pub fn still_matches(&self, project: &Project) -> bool {
        self.fonts_current()
            && self.original.settings == project.settings
            && self.items.iter().all(|id| match (self.original.item(*id), project.item(*id)) {
                (Some(a), Some(b)) => match (&a.kind, &b.kind) {
                    (ItemKind::Comp(a_comp), ItemKind::Comp(b_comp)) => {
                        let same = Arc::ptr_eq(a_comp, b_comp)
                            || (*id == self.target.comp()
                                && self.layer_dependencies.as_ref().is_some_and(|ids| {
                                    same_comp_header(a_comp, b_comp)
                                        && a_comp.layers.iter().filter(|l| ids.contains(&l.id)).map(|l| l.id).eq(b_comp
                                            .layers
                                            .iter()
                                            .filter(|l| ids.contains(&l.id))
                                            .map(|l| l.id))
                                        && ids.iter().all(|id| a_comp.layer(*id) == b_comp.layer(*id))
                                }));
                        same && a.name == b.name && a.proxy == b.proxy
                    }
                    _ => a == b,
                },
                _ => false,
            })
    }

    pub fn verify_media(&self) -> Result<(), String> {
        if !self.fonts_current() {
            return Err("Fonts changed during Smart Freeze rendering; refresh again".into());
        }
        let current = crate::freeze_media::stamps(self.media.iter().map(|stamp| stamp.path.clone()).collect())?;
        for (before, after) in self.media.iter().zip(current) {
            if after != *before {
                return Err(format!("{} changed during Smart Freeze rendering", before.path));
            }
        }
        Ok(())
    }

    pub fn fonts_current(&self) -> bool {
        self.font_generation.is_none_or(|g| g == effectcraft_text::fonts::revision())
    }

    /// Reuse only flat, independent selections, outside both the old and new spans of every
    /// changed layer. Unknown/temporal effect, parent, precomp or media changes render afresh.
    pub fn changed_spans(&self, previous: &Capture) -> Option<Vec<(effectcraft_time::Tick, effectcraft_time::Tick)>> {
        if self.target != previous.target
            || self.media != previous.media
            || self.font_generation != previous.font_generation
            || self.original.settings != previous.original.settings
        {
            return None;
        }
        let Target::Selection { comp, layers } = &self.target else { return None };
        let (before, after) = (previous.original.comp(*comp)?, self.original.comp(*comp)?);
        let (mut a, mut b) = (before.clone(), after.clone());
        a.layers = Default::default();
        b.layers = Default::default();
        if a != b {
            return None;
        }
        let mut spans = vec![];
        for id in layers {
            let (old, new) = (before.layer(*id)?, after.layer(*id)?);
            for layer in [old, new] {
                if layer.parent.is_some()
                    || layer.switches.motion_blur
                    || layer.is_3d()
                    || layer.switches.collapse
                    || matches!(layer.source, LayerSource::Comp { .. })
                    || layer.effects().is_some_and(|g| !g.children.is_empty())
                    || layer.props.sub(effectcraft_project::styles::GROUP).is_some_and(|g| !g.children.is_empty())
                {
                    return None;
                }
            }
            let source_same = old.source.item().and_then(|i| previous.original.item(i)) == new.source.item().and_then(|i| self.original.item(i));
            if !old.same_pixels(new) || !source_same {
                spans.push((old.in_point, old.out_point));
                spans.push((new.in_point, new.out_point));
            }
        }
        Some(spans)
    }
}

/// Lightweight render-worker view of accepted manual caches. Auto versions are deliberately
/// excluded until the scheduler has verified their current dependencies.
pub struct Preview<'a> {
    project: &'a Project,
    sections: Vec<(&'a Section, Vec<LayerId>)>,
    namespace: &'a str,
    feedback: Option<&'a Mutex<Vec<Fault>>>,
    invalid: Vec<(&'a Section, String)>,
}

#[derive(Clone, Debug)]
pub struct Fault {
    pub namespace: String,
    pub section: u64,
    pub directory: String,
    pub message: String,
}
pub type Feedback = Arc<Mutex<Vec<Fault>>>;

/// Compositions whose pixels may be drawn by a viewer. Expressions' property reads do not
/// turn an unrelated composition's preview cache into part of this viewer.
pub fn reachable_comps(project: &Project, root: ItemId) -> BTreeSet<ItemId> {
    let mut seen = BTreeSet::from([root]);
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        let Some(comp) = project.comp(id) else { continue };
        for layer in &comp.layers {
            if let LayerSource::Comp { item } = layer.source
                && seen.insert(item)
            {
                if seen.len() > MAX_ITEMS {
                    return seen;
                }
                pending.push(item);
            }
        }
    }
    seen
}

/// Only fields that change cached pixels participate in viewer identity, not names,
/// measurements, cleanup settings or version history.
pub fn preview_identity(project: &Project, root: ItemId) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    if !project.freeze.sections.iter().any(|s| s.enabled && s.cache.is_some()) {
        return None;
    }
    let scope = reachable_comps(project, root);
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut any = false;
    for section in project.freeze.sections.iter().filter(|s| s.enabled && scope.contains(&s.target.comp())) {
        if let Some(cache) = &section.cache {
            any = true;
            format!(
                "{:?}",
                (
                    &section.target,
                    section.start,
                    section.end,
                    section.follow_comp_range,
                    &cache.directory,
                    &cache.identity,
                    cache.draft,
                    cache.width,
                    cache.height,
                    &cache.render_options,
                    cache.frame_rate
                )
            )
            .hash(&mut hash);
        }
    }
    any.then(|| hash.finish())
}

impl<'a> Preview<'a> {
    pub fn for_comp(project: &'a Project, comp: ItemId) -> Self {
        let scope = reachable_comps(project, comp);
        let mut result = Self::new(project);
        result.sections.retain(|(s, _)| scope.contains(&s.target.comp()));
        result.invalid.retain(|(s, _)| scope.contains(&s.target.comp()));
        result
    }
    pub fn new(project: &'a Project) -> Self {
        let (mut sections, mut invalid) = (vec![], vec![]);
        for section in project.freeze.sections.iter().take(effectcraft_project::freeze::MAX_SECTIONS).filter(|s| s.enabled && !s.auto && s.cache.is_some()) {
            if let Err(error) = section.validate() {
                invalid.push((section, error));
                continue;
            }
            let Some(comp) = project.comp(section.target.comp()) else {
                invalid.push((section, "Frozen composition was removed; remove this freeze section".into()));
                continue;
            };
            if let Err(error) = section.range(comp.duration) {
                invalid.push((section, error));
                continue;
            }
            let ids = match &section.target {
                Target::Selection { layers, .. } => {
                    let ids: Vec<_> = comp.layers.iter().rev().filter(|l| layers.contains(&l.id)).map(|l| l.id).collect();
                    if ids.len() != layers.len() {
                        invalid.push((section, "Frozen layers were removed; remove or recreate this freeze section".into()));
                        continue;
                    }
                    ids
                }
                _ => vec![],
            };
            sections.push((section, ids));
        }
        Self { project, sections, namespace: &project.freeze.namespace, feedback: None, invalid }
    }

    pub fn with_feedback(mut self, feedback: &'a Mutex<Vec<Fault>>) -> Self {
        self.feedback = Some(feedback);
        for (section, error) in &self.invalid {
            self.fault(section, error.clone());
        }
        self
    }

    fn fault(&self, section: &Section, message: String) {
        if let (Some(feedback), Some(version)) = (self.feedback, section.cache.as_ref()) {
            let mut queue = feedback.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.len() < 64 && !queue.iter().any(|f| f.directory == version.directory) {
                queue.push(Fault { namespace: self.namespace.into(), section: section.id, directory: version.directory.clone(), message });
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

impl effectcraft_render::freeze::FreezeSource for Preview<'_> {
    fn selection(&self, comp: ItemId, bottom: LayerId) -> Option<&[LayerId]> {
        self.sections.iter().find(|(s, ids)| s.target.comp() == comp && ids.contains(&bottom)).map(|(_, ids)| ids.as_slice())
    }

    fn frame(
        &self,
        comp: ItemId,
        layers: &[LayerId],
        time: effectcraft_time::Tick,
        opts: &effectcraft_render::RenderOpts,
    ) -> Option<Arc<effectcraft_render::Image>> {
        let duration = self.project.comp(comp)?.duration;
        let (section, _) = self
            .sections
            .iter()
            .find(|(s, ids)| s.target.comp() == comp && ids == layers && s.range(duration).is_ok_and(|(start, end)| time >= start && time < end))?;
        let version = section.cache.as_ref()?;
        if version.frame_rate.num <= 0 || version.frame_rate.den <= 0 {
            return None;
        }
        let stored: effectcraft_render::RenderOpts = serde_json::from_str(&version.render_options).ok()?;
        // Cache resolution is independent of viewer scale. Other rendering settings must
        // still agree; alternate cameras and out-of-comp regions use originals.
        if !opts.scale.is_finite() || opts.scale <= 0.0 || opts.scale > 1.0 {
            return None;
        }
        let current = effectcraft_render::RenderOpts { backend: stored.backend, scale: stored.scale, draft: stored.draft, roi: None, ..*opts };
        if serde_json::to_string(&current).ok()? != version.render_options {
            return None;
        }
        let frame = version.frame_rate.frame_at(time);
        let first = version.frame_rate.frame_at(section.range(duration).ok()?.0);
        if frame.checked_sub(first).and_then(|n| u64::try_from(n).ok()).is_none_or(|n| n >= version.frames) {
            return None;
        }
        // A baked frame cannot stand in for arbitrary sub-frame evaluation (slow precomps,
        // motion-blur samples). Exact-time originals preserve those contexts.
        if version.frame_rate.tick_of(frame) != time {
            return None;
        }
        let path = std::path::Path::new(&version.directory).join(format!("{frame}.ecfreeze"));
        match effectcraft_render::freeze::read_frame(&path) {
            Ok(image) if image.width == version.width && image.height == version.height => {
                let factor = opts.scale / stored.scale;
                if !factor.is_finite() || factor <= 0.0 {
                    return None;
                }
                let w = (f64::from(image.width) * factor).round().max(1.0) as u32;
                let h = (f64::from(image.height) * factor).round().max(1.0) as u32;
                if u64::from(w).saturating_mul(u64::from(h)) > effectcraft_render::freeze::MAX_PIXELS as u64 {
                    return None;
                }
                let image = if w == image.width && h == image.height { image } else { effectcraft_raster::resample(&image, w, h) };
                if let Some(r) = opts.roi {
                    let pixels = r.map(|v| v * opts.scale);
                    if !pixels.iter().all(|v| v.is_finite())
                        || pixels[0] < 0.0
                        || pixels[1] < 0.0
                        || pixels[2] <= 0.0
                        || pixels[3] <= 0.0
                        || pixels[0].fract() != 0.0
                        || pixels[1].fract() != 0.0
                        || pixels[0] + pixels[2] > f64::from(w)
                        || pixels[1] + pixels[3] > f64::from(h)
                    {
                        return None;
                    }
                    return Some(Arc::new(image.crop(
                        pixels[0] as i64,
                        pixels[1] as i64,
                        pixels[2].round().max(1.0) as u32,
                        pixels[3].round().max(1.0) as u32,
                    )));
                }
                Some(Arc::new(image))
            }
            Ok(_) => {
                self.fault(section, "Cache dimensions do not match its version; refresh this section".into());
                None
            }
            Err(e) => {
                self.fault(section, format!("Cache unavailable: {e}. Originals are live; refresh this section."));
                log::warn!("Smart Freeze section {} uses originals: {e}", section.id);
                None
            }
        }
    }
}
