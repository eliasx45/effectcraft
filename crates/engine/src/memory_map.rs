//! The live memory map: what the app's memory holds, by cache and by comp → layer → effect
//! (Window ▸ Memory, the status bar's memory chip, `memory.stats` / `memory.purge`).
//!
//! Cost model (plan/memory/memory-map.md):
//! - [`chip`] reads atomics only (the process's resident size from [`ProcessWatch`], the layer
//!   cache's byte meter, the RAM preview's byte meter): the status bar calls it twice a second.
//! - [`stats`] with `tree` copies every cache's (owner, bytes) out under its lock and aggregates
//!   after it: the Memory panel calls it twice a second **only while it is open**, agents when
//!   they ask.
//!
//! The engine owns the layer cache and the footage source; the frontend owns the RAM preview and
//! the GPU compositor and lends them through [`Hooks`] (closures over its own handles), so the
//! control channel and MCP see the same numbers as the panel.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use effectcraft_project::{ItemId, LayerId, Project};
use effectcraft_render::cache::{EntryUsage, Owner, UsageKind};
use serde::Serialize;

use crate::Session;
use crate::sysinfo::{MemoryPressure, ProcessWatch};

/// The RAM preview's frames (the frontend's viewer frame cache).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewUsage {
    pub bytes: u64,
    /// Of `bytes`, frames kept as GPU textures.
    pub gpu_bytes: u64,
    pub frames: u64,
    pub budget: u64,
    /// Of `bytes`, compressed frame bytes (Compress RAM Preview Frames); the rest is pixels.
    pub packed_bytes: u64,
    /// Of `bytes`, uncompressed pixels (CPU images and GPU textures): `bytes - packed_bytes`.
    pub raw_bytes: u64,
    /// Frames that hold compressed bytes, and what those frames would take uncompressed
    /// (`packedRawBytes / packedBytes` is the compression ratio).
    pub packed_frames: u64,
    pub packed_raw_bytes: u64,
    /// Frames written to the disk cache when they left RAM, and the ones dropped because its
    /// write queue was full.
    pub spilled: u64,
    pub spill_dropped: u64,
    /// Frames read from the disk cache ahead of the playhead instead of rendered.
    pub read_ahead: u64,
    /// Compression is on.
    pub compress: bool,
    /// Per comp (only when asked for detail).
    pub by_comp: Vec<CompFrames>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompFrames {
    pub comp: u64,
    pub bytes: u64,
    pub frames: u64,
}

/// `detail`: also count per comp (iterates the frame keys under the cache's lock).
pub type PreviewProbe = Arc<dyn Fn(bool) -> PreviewUsage + Send + Sync>;
/// Drop the RAM preview's frames of one comp (`Some`) or all (`None`); returns the bytes freed.
pub type PreviewPurge = Arc<dyn Fn(Option<u64>) -> u64 + Send + Sync>;

/// What the frontend lends the memory map (see the module docs). Headless sessions leave the
/// probes empty: their numbers are then the engine's caches only.
#[derive(Clone, Default)]
pub struct Hooks {
    pub process: ProcessWatch,
    pub preview: Option<PreviewProbe>,
    /// The RAM preview's bytes, lock-free (the chip's "tracked" fallback).
    pub preview_meter: Option<Arc<AtomicU64>>,
    pub preview_purge: Option<PreviewPurge>,
    /// Bytes of GPU memory the compositor holds (textures, pools, staging), as the frontend last
    /// read it: while the Memory panel is open twice a second, else every 10 s. The engine holds
    /// no handle on the GPU (a retired device must be free to go).
    pub gpu_meter: Arc<AtomicU64>,
    /// Bytes of effect scratch buffers (the GPU compositor's working images not in a cache),
    /// lock-free: the frontend lends `effectcraft_gpu::effect_scratch()`'s counters. `None`:
    /// not counted (headless sessions).
    pub effect_scratch: Option<Arc<AtomicU64>>,
    /// The most effect scratch bytes at once since the frontend last reset the mark.
    pub effect_scratch_peak: Option<Arc<AtomicU64>>,
}

/// How full memory is (the chip's colour).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Level {
    #[default]
    Ok,
    Warn,
    High,
}

/// The status bar chip: `RAM used / cap GB`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chip {
    /// The process's resident bytes, or (when the system doesn't say) the bytes the caches track.
    pub used: u64,
    /// `used` is the caches' tracked bytes, not the process's.
    pub tracked: bool,
    /// What the app may use: the machine's memory minus RAM Reserved for Other Applications.
    pub cap: Option<u64>,
    pub pressure: Option<MemoryPressure>,
    pub level: Level,
}

/// Physical memory minus RAM Reserved for Other Applications (`None`: memory unknown).
pub fn cap(s: &Session) -> Option<u64> {
    let total = s.sys_memory.map(|m| m.total).filter(|t| *t > 0)?;
    Some(total.saturating_sub(u64::from(s.prefs.memory.ram_reserved_gb) << 30))
}

/// The chip's level: the system's pressure when it reports one, else how close `used` is to
/// `cap` (75 % warn, 90 % high).
pub fn level(used: u64, cap: Option<u64>, pressure: Option<MemoryPressure>) -> Level {
    let by_use = match cap.filter(|c| *c > 0) {
        Some(c) if used.saturating_mul(10) >= c.saturating_mul(9) => Level::High,
        Some(c) if used.saturating_mul(4) >= c.saturating_mul(3) => Level::Warn,
        _ => Level::Ok,
    };
    let by_pressure = match pressure {
        Some(MemoryPressure::Critical) => Level::High,
        Some(MemoryPressure::Warning) => Level::Warn,
        _ => Level::Ok,
    };
    by_use.max(by_pressure)
}

/// The chip's numbers: atomic loads only, nothing walks a cache. Call [`ProcessWatch::poll`]
/// (through `s.memory_map.process`) every couple of seconds to keep `used` current.
pub fn chip(s: &Session) -> Chip {
    let h = &s.memory_map;
    let resident = h.process.resident();
    let used = resident.unwrap_or_else(|| tracked_now(s));
    let cap = cap(s);
    let pressure = h.process.pressure().or_else(|| s.memory_watch.pressure());
    Chip { used, tracked: resident.is_none(), cap, pressure, level: level(used, cap, pressure) }
}

/// The bytes the caches track that can be read without a lock (layer cache + RAM preview +
/// effect scratch).
pub fn tracked_now(s: &Session) -> u64 {
    let load = |m: &Option<Arc<AtomicU64>>| m.as_ref().map(|m| m.load(Ordering::Relaxed)).unwrap_or(0);
    let h = &s.memory_map;
    s.layer_cache.bytes_now().saturating_add(load(&h.preview_meter)).saturating_add(load(&h.effect_scratch))
}

/// Bytes per category (the Memory panel's stacked bar).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Categories {
    /// RAM preview frames (CPU and GPU).
    pub preview: u64,
    /// The layer cache's CPU buffers.
    pub layer: u64,
    /// Decoded footage frames.
    pub media: u64,
    /// The GPU compositor's memory, less the preview's GPU frames (counted under `preview`).
    pub gpu: u64,
    /// Effect scratch buffers (`None`: not counted yet).
    pub effect_scratch: Option<u64>,
    /// The process's resident size less everything above (`None`: resident size unknown).
    pub untracked: Option<u64>,
}

impl Categories {
    pub fn tracked(&self) -> u64 {
        [self.preview, self.layer, self.media, self.gpu, self.effect_scratch.unwrap_or(0)].into_iter().fold(0, u64::saturating_add)
    }
}

/// A comp, layer or effect row of the tree (comps hold layers, layers hold effects).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    /// Comp item id, layer id, or 0-based effect index.
    pub id: u64,
    pub name: String,
    /// Everything below: `cache_bytes + gpu_bytes + preview_bytes`.
    pub bytes: u64,
    /// Layer cache CPU buffers.
    pub cache_bytes: u64,
    /// Layer cache GPU textures.
    pub gpu_bytes: u64,
    /// Cached buffers (CPU entries and GPU slots).
    pub entries: u64,
    /// RAM preview frames (comps only).
    pub preview_bytes: u64,
    pub frames: u64,
    pub children: Vec<Row>,
}

impl Row {
    fn add(&mut self, u: &EntryUsage) {
        let b = u.bytes as u64;
        match u.kind {
            UsageKind::Cpu => self.cache_bytes = self.cache_bytes.saturating_add(b),
            UsageKind::Gpu => self.gpu_bytes = self.gpu_bytes.saturating_add(b),
        }
        self.bytes = self.bytes.saturating_add(b);
        self.entries += 1;
    }
}

/// Group cached buffers and preview frames by comp → layer → effect, named from `project`
/// (a comp or layer that is gone keeps its id in its name), most bytes first. Returns the comps
/// and the unattributed buffers (`id` 0).
pub fn aggregate(entries: &[EntryUsage], preview: &[CompFrames], project: &Project) -> (Vec<Row>, Row) {
    #[derive(Default)]
    struct Layer {
        row: Row,
        effects: BTreeMap<u32, Row>,
    }
    #[derive(Default)]
    struct Comp {
        row: Row,
        layers: BTreeMap<u64, Layer>,
    }
    let mut comps: BTreeMap<u64, Comp> = BTreeMap::new();
    let mut loose = Row { name: "Unattributed".into(), ..Row::default() };
    for u in entries {
        let Owner { comp: Some(c), layer, effect } = u.owner else {
            loose.add(u);
            continue;
        };
        let comp = comps.entry(c).or_default();
        comp.row.add(u);
        let Some(l) = layer else { continue };
        let layer = comp.layers.entry(l).or_default();
        layer.row.add(u);
        if let Some(e) = effect {
            layer.effects.entry(e).or_default().add(u);
        }
    }
    for f in preview {
        let comp = comps.entry(f.comp).or_default();
        comp.row.preview_bytes = comp.row.preview_bytes.saturating_add(f.bytes);
        comp.row.bytes = comp.row.bytes.saturating_add(f.bytes);
        comp.row.frames = comp.row.frames.saturating_add(f.frames);
    }
    let by_bytes = |a: &Row, b: &Row| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id));
    let mut out: Vec<Row> = comps
        .into_iter()
        .map(|(cid, c)| {
            let pc = project.comp(ItemId(cid));
            let mut layers: Vec<Row> = c
                .layers
                .into_iter()
                .map(|(lid, l)| {
                    let pl = pc.and_then(|pc| pc.layer(LayerId(lid)));
                    let mut effects: Vec<Row> = l
                        .effects
                        .into_iter()
                        .map(|(ei, mut r)| {
                            let name = pl.and_then(|pl| pl.effects()).and_then(|fx| fx.groups().nth(ei as usize)).map(|g| g.name.clone());
                            r.id = u64::from(ei);
                            r.name = format!("Input to {}", name.unwrap_or_else(|| format!("effect {}", ei.saturating_add(1))));
                            r
                        })
                        .collect();
                    effects.sort_by(by_bytes);
                    Row { id: lid, name: pl.map(|pl| pl.name.clone()).unwrap_or_else(|| format!("Layer {lid} (gone)")), children: effects, ..l.row }
                })
                .collect();
            layers.sort_by(by_bytes);
            let name = project.item(ItemId(cid)).map(|i| i.name.clone()).unwrap_or_else(|| format!("Comp {cid} (gone)"));
            Row { id: cid, name, children: layers, ..c.row }
        })
        .collect();
    out.sort_by(by_bytes);
    (out, loose)
}

/// Everything the Memory panel shows (`memory.stats`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStats {
    pub chip: Chip,
    pub categories: Categories,
    pub preview: PreviewUsage,
    pub layer_cache_budget: u64,
    pub layer_cache_entries: u64,
    pub media_frames: u64,
    pub media_budget: Option<u64>,
    /// Decoded footage by file: (path, bytes, frames), most bytes first.
    pub media_files: Vec<(String, usize, usize)>,
    /// comp → layer → effect (`tree` only).
    pub comps: Vec<Row>,
    /// Buffers no comp is known for (`tree` only).
    pub unattributed: Option<Row>,
    /// The most effect scratch bytes at once (`categories.effectScratch` is the current bytes).
    pub effect_scratch_peak: Option<u64>,
    /// The shared cache budget (Share One Budget Between the Caches): the total, and per cache
    /// what it holds now, its floor, its limit, its own budget (`share`) and the bytes the
    /// other caches took from it (`evicted`). `None` while each cache keeps its own budget.
    pub shared_budget: Option<effectcraft_render::cache::shared::SharedUsage>,
}

/// The memory map now. `tree`: also the comp → layer → effect breakdown (copies every cache's
/// keys out under its lock; only while the panel is open or an agent asks).
pub fn stats(s: &Session, tree: bool) -> MemoryStats {
    let h = &s.memory_map;
    let chip = chip(s);
    let preview = h.preview.as_ref().map(|p| p(tree)).unwrap_or_default();
    let media = s.footage.cache_usage().unwrap_or_default();
    let usage = if tree { s.layer_cache.usage() } else { vec![] };
    let layer = s.layer_cache.bytes_now();
    let gpu = h.gpu_meter.load(Ordering::Relaxed).saturating_sub(preview.gpu_bytes);
    let effect_scratch = h.effect_scratch.as_ref().map(|m| m.load(Ordering::Relaxed));
    let mut categories = Categories { preview: preview.bytes, layer, media: media.bytes as u64, gpu, effect_scratch, untracked: None };
    categories.untracked = h.process.resident().map(|r| r.saturating_sub(categories.tracked()));
    let (comps, unattributed) = if tree {
        let (c, u) = aggregate(&usage, &preview.by_comp, &s.project);
        (c, Some(u))
    } else {
        (vec![], None)
    };
    let st = s.layer_cache.stats();
    MemoryStats {
        chip,
        categories,
        layer_cache_budget: s.layer_cache.budget() as u64,
        layer_cache_entries: st.entries.saturating_add(st.gpu_entries) as u64,
        media_frames: media.frames as u64,
        media_budget: s.footage.cache_budget().map(|b| b as u64),
        media_files: media.by_file,
        preview,
        comps,
        unattributed,
        effect_scratch_peak: h.effect_scratch_peak.as_ref().map(|m| m.load(Ordering::Relaxed)),
        shared_budget: s.cache_share.is_on().then(|| s.cache_share.usage()),
    }
}

/// What [`purge`] freed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Freed {
    pub layer_bytes: u64,
    pub layer_entries: u64,
    pub gpu_slots: u64,
    pub preview_bytes: u64,
    pub media_bytes: u64,
}

/// Which caches [`purge`] empties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    All,
    Layers,
    Preview,
    Media,
}

impl What {
    pub fn parse(s: &str) -> Option<What> {
        match s {
            "all" => Some(What::All),
            "layers" | "layer" => Some(What::Layers),
            "preview" => Some(What::Preview),
            "media" | "footage" => Some(What::Media),
            _ => None,
        }
    }
}

/// Purge the caches of one comp (and one of its layers), or everything. Decoded footage is
/// shared between comps, so only a purge of everything empties it; the RAM preview keeps frames
/// per comp, so a layer's purge leaves them. Everything in the RAM preview goes the way
/// Edit ▸ Purge does ([`crate::Event::PurgeCaches`]: frames rendering now are dropped too).
pub fn purge(s: &mut Session, comp: Option<u64>, layer: Option<u64>, what: What) -> Freed {
    let mut f = Freed::default();
    let everything = comp.is_none() && layer.is_none();
    if matches!(what, What::All | What::Layers) {
        if everything {
            let before = s.layer_cache.stats();
            s.layer_cache.clear();
            f.layer_bytes = before.bytes as u64;
            f.layer_entries = before.entries as u64;
            f.gpu_slots = before.gpu_entries as u64;
        } else {
            let p = s.layer_cache.purge_owned(|o| o.matches(comp, layer));
            f.layer_bytes = p.bytes as u64;
            f.layer_entries = p.entries as u64;
            f.gpu_slots = p.gpu_slots as u64;
        }
    }
    if matches!(what, What::All | What::Preview) && everything {
        f.preview_bytes = s.memory_map.preview.as_ref().map(|p| p(false).bytes).unwrap_or(0);
        s.events.push(crate::Event::PurgeCaches);
    } else if matches!(what, What::All | What::Preview)
        && layer.is_none()
        && let Some(p) = &s.memory_map.preview_purge
    {
        f.preview_bytes = p(comp);
    }
    if matches!(what, What::All | What::Media) && everything {
        let before = s.footage.cache_usage().map(|u| u.bytes).unwrap_or(0);
        s.footage.purge();
        let after = s.footage.cache_usage().map(|u| u.bytes).unwrap_or(0);
        f.media_bytes = before.saturating_sub(after) as u64;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(comp: Option<u64>, layer: Option<u64>, effect: Option<u32>, bytes: usize, kind: UsageKind) -> EntryUsage {
        EntryUsage { owner: Owner { comp, layer, effect }, bytes, kind, probation: false }
    }

    #[test]
    fn aggregates_by_comp_layer_and_effect() {
        let entries = [
            usage(Some(1), Some(10), None, 100, UsageKind::Cpu),
            usage(Some(1), Some(10), None, 50, UsageKind::Gpu),
            usage(Some(1), Some(10), Some(2), 30, UsageKind::Cpu),
            usage(Some(1), Some(11), None, 500, UsageKind::Cpu),
            usage(Some(2), Some(20), None, 7, UsageKind::Cpu),
            usage(None, None, None, 9, UsageKind::Cpu),
        ];
        let preview = [CompFrames { comp: 2, bytes: 1000, frames: 4 }, CompFrames { comp: 3, bytes: 5, frames: 1 }];
        let (comps, loose) = aggregate(&entries, &preview, &Project::default());
        // Most bytes first: comp 2 (1007 with its preview), comp 1 (680), comp 3 (5).
        assert_eq!(comps.iter().map(|c| (c.id, c.bytes)).collect::<Vec<_>>(), vec![(2, 1007), (1, 680), (3, 5)]);
        let c1 = &comps[1];
        assert_eq!((c1.cache_bytes, c1.gpu_bytes, c1.entries, c1.preview_bytes), (630, 50, 4, 0));
        assert_eq!(c1.children.iter().map(|l| (l.id, l.bytes)).collect::<Vec<_>>(), vec![(11, 500), (10, 180)]);
        let l10 = &c1.children[1];
        assert_eq!((l10.cache_bytes, l10.gpu_bytes, l10.entries), (130, 50, 3));
        assert_eq!(l10.children.len(), 1);
        assert_eq!((l10.children[0].id, l10.children[0].bytes), (2, 30));
        assert!(l10.children[0].name.starts_with("Input to "));
        let c2 = &comps[0];
        assert_eq!((c2.preview_bytes, c2.frames, c2.cache_bytes), (1000, 4, 7));
        // Names of things the project doesn't have keep their ids.
        assert_eq!(c2.name, "Comp 2 (gone)");
        assert_eq!(c2.children[0].name, "Layer 20 (gone)");
        assert_eq!((loose.bytes, loose.entries), (9, 1));
        // Every byte lands exactly once.
        let total: u64 = comps.iter().map(|c| c.bytes).sum::<u64>() + loose.bytes;
        assert_eq!(total, entries.iter().map(|e| e.bytes as u64).sum::<u64>() + 1005);
    }

    #[test]
    fn level_follows_use_and_pressure() {
        let gb = 1u64 << 30;
        assert_eq!(level(10 * gb, Some(54 * gb), None), Level::Ok);
        assert_eq!(level(41 * gb, Some(54 * gb), None), Level::Warn);
        assert_eq!(level(50 * gb, Some(54 * gb), None), Level::High);
        assert_eq!(level(10 * gb, Some(54 * gb), Some(MemoryPressure::Warning)), Level::Warn);
        assert_eq!(level(10 * gb, None, Some(MemoryPressure::Critical)), Level::High);
        assert_eq!(level(10 * gb, None, None), Level::Ok);
    }

    #[test]
    fn purges_one_comp_and_keeps_the_meter_exact() {
        let mut s = Session::new();
        let buf = || Arc::new(effectcraft_effects::Buf { img: effectcraft_render::Image::new(8, 8), offset: [0.0; 2], scale: 1.0 });
        {
            let _o = effectcraft_render::cache::OwnerScope::layer(1, 10);
            s.layer_cache.insert(1, buf());
        }
        {
            let _o = effectcraft_render::cache::OwnerScope::layer(2, 20);
            s.layer_cache.insert(2, buf());
        }
        let one = s.layer_cache.stats().bytes as u64 / 2;
        let st = stats(&s, true);
        assert_eq!(st.categories.layer, 2 * one);
        assert_eq!(st.comps.len(), 2);
        let f = purge(&mut s, Some(1), None, What::All);
        assert_eq!((f.layer_entries, f.layer_bytes), (1, one));
        assert_eq!(s.layer_cache.bytes_now(), one);
        let st = stats(&s, true);
        assert_eq!(st.comps.iter().map(|c| c.id).collect::<Vec<_>>(), vec![2]);
        purge(&mut s, None, None, What::All);
        assert_eq!(s.layer_cache.bytes_now(), 0);
        assert_eq!(tracked_now(&s), 0);
    }

    #[test]
    fn memory_stats_report_the_shared_budget_split_live() {
        let mut s = Session::new();
        s.prefs_changed();
        let st = stats(&s, false);
        let shared = st.shared_budget.expect("on by default");
        let b = s.prefs.cache_budgets(s.sys_memory);
        let split = b.shared.expect("shared split");
        assert_eq!(shared.total, split.total);
        assert_eq!((shared.layer.floor, shared.media.floor, shared.preview.floor), (split.floors[0], split.floors[1], split.floors[2]));
        assert!(shared.layer.joined, "the layer cache takes part");
        assert_eq!(shared.layer.bytes, 0);
        // A buffer in the layer cache shows in its share at once.
        let buf = Arc::new(effectcraft_effects::Buf { img: effectcraft_render::Image::new(8, 8), offset: [0.0; 2], scale: 1.0 });
        s.layer_cache.insert(1, buf);
        let st = stats(&s, false);
        assert_eq!(st.shared_budget.map(|u| u.layer.bytes), Some(s.layer_cache.bytes_now()));
        let j = serde_json::to_value(&st).unwrap_or_default();
        assert!(j["sharedBudget"]["layer"]["floor"].is_u64());
        assert!(j["sharedBudget"]["preview"]["limit"].is_u64());
        // Off: no shared section.
        s.prefs.memory.shared_cache_budget = false;
        s.prefs_changed();
        assert!(stats(&s, false).shared_budget.is_none());
    }

    #[test]
    fn memory_effect_scratch_counts_in_the_categories_tracked_total_and_json() {
        let mut s = Session::new();
        let (live, peak) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        s.memory_map.effect_scratch = Some(live.clone());
        s.memory_map.effect_scratch_peak = Some(peak.clone());
        live.store(3 << 20, Ordering::Relaxed);
        peak.store(5 << 20, Ordering::Relaxed);
        let st = stats(&s, false);
        assert_eq!(st.categories.effect_scratch, Some(3 << 20));
        assert_eq!(st.effect_scratch_peak, Some(5 << 20));
        assert_eq!(tracked_now(&s), 3 << 20);
        let j = serde_json::to_value(&st).unwrap_or_default();
        assert_eq!(j["categories"]["effectScratch"], serde_json::json!(3u64 << 20));
        assert_eq!(j["effectScratchPeak"], serde_json::json!(5u64 << 20));
    }
}
