//! Viewer frames found again by what they show: every frame a worker renders is stamped with
//! its fingerprint ([`Renderer::frame_fingerprint`]: a hash of everything its pixels are made
//! of at its time, keyframes left out where only the values at that time matter). After an
//! edit, a frame of the new content whose fingerprint matches a cached frame of the same comp,
//! frame, scale, view and render options takes that frame over instead of rendering again: a
//! keyframe edit re-renders only the frames between its neighbouring keyframes, an effect edit
//! only the frames where the effect's values changed, and undo or redo finds every frame of the
//! state it returns to (not only the last [`super::MAX_KEEPERS`] identities').
//!
//! Frames are taken over when they are asked for (a worker computes the fingerprint before
//! rendering, which costs far less than a render) and, right after an edit, by a sweep over the
//! cached frames of the series the viewer asks for, nearest the playhead first, so the cache bar
//! stays green where the edit changed nothing. A frame without a fingerprint (one that can't be
//! told: an expression meeting a time reader, a frozen preview) is invalidated as before.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use effectcraft_engine::project::ItemId;
use effectcraft_engine::render::{RenderOpts, Renderer};
use effectcraft_engine::time::Tick;

use super::{Cache, FrameKey, Frames, RenderSource};

/// A frame's place (comp, frame, scale, view, options) and fingerprint.
type PrintKey = (u64, i64, u32, u64, u64, u64);

fn print_key(k: &FrameKey, print: u64) -> PrintKey {
    (k.comp, k.frame, k.scale, k.view, k.opts, print)
}

/// Cached frames' fingerprints, both ways.
#[derive(Default)]
pub(super) struct Prints {
    of: HashMap<FrameKey, u64>,
    by: HashMap<PrintKey, FrameKey>,
}

impl Prints {
    /// Frame `k` shows `print`.
    pub(super) fn stamp(&mut self, k: FrameKey, print: u64) {
        self.remove(&k);
        self.of.insert(k, print);
        self.by.insert(print_key(&k, print), k);
    }

    pub(super) fn remove(&mut self, k: &FrameKey) {
        if let Some(p) = self.of.remove(k) {
            let pk = print_key(k, p);
            if self.by.get(&pk).is_some_and(|o| o == k) {
                self.by.remove(&pk);
            }
        }
    }

    /// The fingerprint of frame `from` now belongs to `to`.
    pub(super) fn rename(&mut self, from: &FrameKey, to: FrameKey) {
        if let Some(p) = self.of.get(from).copied() {
            self.remove(from);
            self.stamp(to, p);
        }
    }

    pub(super) fn has(&self, k: &FrameKey) -> bool {
        self.of.contains_key(k)
    }

    #[cfg(test)]
    pub(super) fn get(&self, k: &FrameKey) -> Option<u64> {
        self.of.get(k).copied()
    }
}

impl Cache {
    /// Move the frame of `from` to `to` (its pixels, place in line, protection and
    /// fingerprint; byte counts are unchanged).
    pub(super) fn rename(&mut self, from: FrameKey, to: FrameKey) {
        if from == to || self.map.contains_key(&to) {
            return;
        }
        let Some(img) = self.map.remove(&from) else { return };
        self.map.insert(to, img);
        self.move_draft(&from, to);
        self.order.rename(&from, to);
        if let Some(rank) = self.protected.remove(&from) {
            self.protected.insert(to, rank);
        }
        self.prints.rename(&from, to);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Make the cached frame that shows `print` at `key`'s place the frame of `key`: whether
    /// `key` is cached now.
    pub(super) fn adopt(&mut self, key: FrameKey, print: u64) -> bool {
        // A draft of the cloud's stream under the key gives way to an exact frame.
        let draft = self.drafts.contains(&key);
        if self.map.contains_key(&key) && !draft {
            return true;
        }
        let Some(from) = self.prints.by.get(&print_key(&key, print)).copied() else { return false };
        if from == key || !self.map.contains_key(&from) {
            return false;
        }
        if draft {
            self.order.remove(&key);
            self.remove(&key);
        }
        self.rename(from, key);
        self.map.contains_key(&key)
    }
}

/// The identity frames of the dropped identity `content` keep (see `Cache::drop_contents`): one
/// no project is given, so only a fingerprint finds them.
pub(super) fn orphan(content: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (0x0f2a_17e5_u64, content).hash(&mut h);
    h.finish()
}

/// The fingerprint of the frame of `comp` at `t` rendered from `src` with `opts`, or `None`
/// when it can't be told (or telling it panicked).
pub(super) fn of(src: &RenderSource, comp: ItemId, t: Tick, opts: RenderOpts) -> Option<u64> {
    if !effectcraft_engine::freeze::Preview::for_comp(&src.project, comp).is_empty() {
        return None;
    }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut r = Renderer::new(&src.project, src.footage.as_ref(), opts);
        r.expr = src.expr.as_deref();
        r.frame_fingerprint(comp, t)
    }))
    .ok()
    .flatten()
}

/// Frames one sweep looks at, at most.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
const MAX_SWEEP: usize = 4096;

/// Time one sweep may take.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
const SWEEP_TIME: std::time::Duration = std::time::Duration::from_secs(2);

/// The last sweep: (series with frame 0, project revision) per comp, and a counter that tells a
/// running sweep it was superseded.
#[derive(Default)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) struct Sweeps {
    last: Mutex<HashMap<u64, (FrameKey, u64)>>,
    generation: Arc<AtomicU64>,
}

impl Frames {
    /// After an edit (a new revision), take over the cached frames of `key`'s series that show
    /// what its frames show now (see the module docs), on the frame pool. Once per series and
    /// revision; a newer sweep stops an older one.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn sweep(&self, src: &RenderSource, key: FrameKey, comp: ItemId, opts: RenderOpts) {
        let series = FrameKey { frame: 0, ..key };
        {
            let Ok(mut last) = self.sweeps.last.lock() else { return };
            if last.get(&key.comp).is_some_and(|(s, r)| *s == series && *r == key.revision) {
                return;
            }
            if last.len() > 64 {
                last.clear();
            }
            last.insert(key.comp, (series, key.revision));
        }
        let Some(fps) = src.project.comp(comp).map(|c| c.frame_rate) else { return };
        let generation = self.sweeps.generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel).wrapping_add(1);
        let current = self.sweeps.generation.clone();
        let cache = self.cache.clone();
        let ctx = self.ctx.clone();
        let src = src.clone();
        let center = key.frame;
        self.worker().spawn(move || {
            let t0 = web_time::Instant::now();
            let mut frames: Vec<i64> = {
                let Ok(c) = cache.lock() else { return };
                c.prints
                    .of
                    .keys()
                    .filter(|k| {
                        k.comp == series.comp && k.scale == series.scale && k.view == series.view && k.opts == series.opts && k.content != series.content
                    })
                    .map(|k| k.frame)
                    .collect()
            };
            frames.sort_unstable_by_key(|f| (f.abs_diff(center), *f));
            frames.dedup();
            frames.truncate(MAX_SWEEP);
            let mut adopted = 0usize;
            for f in frames {
                if current.load(std::sync::atomic::Ordering::Acquire) != generation || t0.elapsed() > SWEEP_TIME {
                    break;
                }
                let target = FrameKey { frame: f, ..series };
                if cache.lock().map(|c| c.map.contains_key(&target)).unwrap_or(true) {
                    continue;
                }
                let Some(print) = of(&src, comp, fps.tick_of(f), opts) else { continue };
                if cache.lock().is_ok_and(|mut c| c.adopt(target, print)) {
                    adopted += 1;
                    // The cache bar shows them as they come.
                    if adopted % 64 == 1
                        && let Some(ctx) = &ctx
                    {
                        ctx.request_repaint();
                    }
                }
            }
            if adopted > 0
                && let Some(ctx) = &ctx
            {
                ctx.request_repaint();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{FrameImage, to_color_image};
    use effectcraft_engine::render::{LayerCache, NoFootage};
    use effectcraft_engine::{Session, project::Project};
    use effectcraft_keyframe::{Keyframe, Value};
    use serde_json::json;

    /// A 64×64, 4 s, 30 fps comp with a 3D solid whose opacity has keyframes at 0, 1, 2 and 3 s.
    fn scene() -> (Arc<Project>, ItemId) {
        let mut s = Session::default();
        let id =
            ItemId(s.execute("comp.new", json!({"name":"Prints", "width":64, "height":64, "duration":4, "frameRate":30})).unwrap()["comp"].as_u64().unwrap());
        s.execute("layer.newSolid", json!({"comp":id.0,"color":"#e02060"})).unwrap();
        let p = Arc::make_mut(&mut s.project);
        // 3D: RT5's single-property proof (property_reuse) declines it, so only fingerprints
        // keep frames here.
        p.comp_mut(id).unwrap().layers.get_mut(0).unwrap().switches.three_d = true;
        let opacity = p.comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut("transform/opacity").unwrap();
        opacity.keys =
            [0., 1., 2., 3.].into_iter().enumerate().map(|(i, t)| Keyframe::new(Tick::from_seconds_f64(t), Value::Scalar(100. - 20. * i as f64))).collect();
        (s.project, id)
    }

    fn source(project: &Arc<Project>) -> RenderSource {
        RenderSource {
            freeze_feedback: Default::default(),
            project: project.clone(),
            footage: Arc::new(NoFootage),
            expr: None,
            layer_cache: Arc::new(LayerCache::new(64 << 20)),
            gpu: None,
            gpu_display: false,
            disk: None,
        }
    }

    fn key(f: &Frames, project: &Arc<Project>, revision: u64, id: ItemId, frame: i64) -> FrameKey {
        let content = f.content_of(project, revision, id);
        FrameKey { revision, content, comp: id.0, frame, scale: 1000, view: 0, opts: crate::frames::opts_hash(&RenderOpts::default()) }
    }

    /// Frame `frame`'s time, as the viewer asks for it.
    fn at(frame: i64) -> Tick {
        effectcraft_engine::time::FrameRate::FPS_30.tick_of(frame)
    }

    const FRAMES: [i64; 13] = [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 119];

    fn request_all(f: &Frames, project: &Arc<Project>, revision: u64, id: ItemId) {
        let src = source(project);
        for frame in FRAMES {
            f.request(&src, key(f, project, revision, id, frame), id, at(frame), RenderOpts::default());
        }
        wait(|| f.inflight() == 0);
    }

    fn wait(done: impl Fn() -> bool) {
        for _ in 0..2000 {
            if done() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The cached frames of `project` at `revision`, after asking for one frame (the sweep).
    fn swept(f: &Frames, project: &Arc<Project>, revision: u64, id: ItemId, ask: i64, expect: usize) -> Vec<i64> {
        let src = source(project);
        f.request(&src, key(f, project, revision, id, ask), id, at(ask), RenderOpts::default());
        let cached = || FRAMES.into_iter().filter(|fr| f.is_cached(&key(f, project, revision, id, *fr))).collect::<Vec<_>>();
        wait(|| f.inflight() == 0 && cached().len() >= expect);
        cached()
    }

    fn pixels(f: &Frames, k: &FrameKey) -> Vec<egui::Color32> {
        match f.cache.lock().unwrap().map.get(k).unwrap() {
            FrameImage::Cpu(c) => c.pixels.clone(),
            FrameImage::Gpu(_) => panic!("CPU frames"),
        }
    }

    fn edit(before: &Arc<Project>, id: ItemId, key: usize, value: f64) -> Arc<Project> {
        let mut after = before.clone();
        let p = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap().props.prop_mut("transform/opacity").unwrap();
        p.keys[key].value = Value::Scalar(value);
        after
    }

    /// Editing the keyframe at 1 s keeps the frames outside 0–2 s (found by the sweep the next
    /// request starts, without rendering them), each equal to a fresh render; undo and redo then
    /// find every frame of the state they return to.
    #[test]
    fn an_edit_keeps_the_frames_it_doesnt_change_and_undo_redo_find_theirs() {
        let (before, id) = scene();
        let f = Frames::default();
        request_all(&f, &before, 1, id);
        assert!(FRAMES.iter().all(|fr| f.is_cached(&key(&f, &before, 1, id, *fr))));
        let bytes = f.cache.lock().unwrap().bytes;
        let after = edit(&before, id, 1, 10.);
        // Ask for frame 100 only: the sweep takes over the others the edit didn't change.
        let kept = swept(&f, &after, 2, id, 100, 8);
        assert_eq!(kept, vec![0, 60, 70, 80, 90, 100, 110, 119], "the frames outside 0–2 s");
        for fr in &kept {
            let expected = to_color_image(&effectcraft_engine::render::render_frame(&after, id, at(*fr), 1.));
            assert_eq!(pixels(&f, &key(&f, &after, 2, id, *fr)), expected.pixels, "frame {fr} equals a fresh render");
        }
        assert_eq!(f.cache.lock().unwrap().bytes, bytes, "moved, not copied");
        // Render the changed ones too, then undo: every frame of the first state comes back.
        request_all(&f, &after, 2, id);
        let undo = swept(&f, &before, 3, id, 0, FRAMES.len());
        assert_eq!(undo, FRAMES.to_vec());
        for fr in FRAMES {
            let expected = to_color_image(&effectcraft_engine::render::render_frame(&before, id, at(fr), 1.));
            assert_eq!(pixels(&f, &key(&f, &before, 3, id, fr)), expected.pixels, "undo: frame {fr}");
        }
        // Redo: the edited frames (rendered at revision 2) come back too.
        let redo = swept(&f, &after, 4, id, 0, FRAMES.len());
        assert_eq!(redo, FRAMES.to_vec());
    }

    /// A frame asked for after an edit takes over a cached frame that shows the same (no sweep
    /// needed): the worker compares fingerprints before it renders.
    #[test]
    fn a_requested_frame_takes_over_the_cached_one_it_matches() {
        let (before, id) = scene();
        let f = Frames::default();
        request_all(&f, &before, 1, id);
        let after = edit(&before, id, 1, 10.);
        let k = key(&f, &after, 2, id, 90);
        let old = key(&f, &before, 1, id, 90);
        let print = f.cache.lock().unwrap().prints.get(&old).unwrap();
        assert!(f.cache.lock().unwrap().adopt(k, print));
        assert!(f.is_cached(&k) && !f.is_cached(&old));
        assert_eq!(f.cache.lock().unwrap().prints.get(&k), Some(print));
        // A frame the edit changed has another fingerprint: nothing to take over.
        let changed = of(&source(&after), id, Tick::from_seconds_f64(1.), RenderOpts::default()).unwrap();
        assert!(!f.cache.lock().unwrap().adopt(key(&f, &after, 2, id, 30), changed));
    }

    /// Where the fingerprint can't prove anything (time remapping keeps its keyframes in the
    /// key) an edit invalidates every frame, as before.
    #[test]
    fn a_time_remap_edit_keeps_nothing() {
        let (mut before, id) = scene();
        {
            let p = Arc::make_mut(&mut before);
            let l = p.comp_mut(id).unwrap().layers.get_mut(0).unwrap();
            let mut tr = l.props.prop("transform/opacity").unwrap().clone();
            tr.match_id = "timeRemap".into();
            tr.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(0.)), Keyframe::new(Tick::from_seconds_f64(4.), Value::Scalar(4.))];
            l.props.children.push(effectcraft_engine::project::Node::Prop(tr));
        }
        let f = Frames::default();
        request_all(&f, &before, 1, id);
        let mut after = before.clone();
        let l = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(0).unwrap();
        l.props.get_mut("timeRemap").unwrap().keys[1].value = Value::Scalar(3.);
        let src = source(&after);
        f.request(&src, key(&f, &after, 2, id, 0), id, Tick::ZERO, RenderOpts::default());
        wait(|| f.inflight() == 0);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let kept: Vec<i64> = FRAMES.into_iter().filter(|fr| *fr != 0 && f.is_cached(&key(&f, &after, 2, id, *fr))).collect();
        assert_eq!(kept, Vec::<i64>::new());
    }

    /// A cloud stand-in (eager, like the cloud server): renders each frame here on the CPU and
    /// keeps every request's frame and disk key, so a test sees what would have downloaded.
    #[derive(Default)]
    struct Cloud {
        sent: Mutex<Vec<(i64, Option<u128>)>>,
    }

    impl crate::frames::RemoteFrames for Cloud {
        fn slots(&self) -> usize {
            4
        }
        fn eager(&self) -> bool {
            true
        }
        fn gpu(&self) -> bool {
            true
        }
        fn disk(&self) -> bool {
            true
        }
        fn start(&self, job: crate::frames::RemoteJob, done: crate::frames::RemoteDone) {
            let fps = job.project.comp(job.comp).unwrap().frame_rate;
            self.sent.lock().unwrap().push((fps.frame_at(job.t), job.disk_key));
            let img = to_color_image(&effectcraft_engine::render::render_frame(&job.project, job.comp, job.t, job.opts.scale));
            let rgba = img.pixels.iter().flat_map(|p| p.to_array()).collect();
            done(Ok(crate::frames::RemoteFrame { width: img.size[0] as u32, height: img.size[1] as u32, rgba, ms: 0. }));
        }
    }

    fn with_cloud() -> (Frames, Arc<Cloud>) {
        let cloud = Arc::new(Cloud::default());
        let mut f = Frames::default();
        f.set_remote(Some(cloud.clone()));
        (f, cloud)
    }

    /// Ask for every frame of `FRAMES` through the cloud and wait until they are in.
    fn request_all_cloud(f: &Frames, project: &Arc<Project>, revision: u64, id: ItemId) {
        let src = source(project);
        for frame in FRAMES {
            f.request(&src, key(f, project, revision, id, frame), id, at(frame), RenderOpts::default());
            f.dispatch_remote();
        }
        wait(|| {
            f.dispatch_remote();
            f.inflight() == 0
        });
    }

    /// With the cloud rendering, an edit downloads only the frames it changed: the frames that
    /// came from the cloud carry their fingerprints, so the ones outside the edited keyframe's
    /// neighbours (0–2 s) are taken over in RAM, each equal to a fresh render; the frames sent
    /// again are exactly those between the neighbours.
    #[test]
    fn with_the_cloud_an_edit_downloads_only_the_frames_it_changes() {
        let (before, id) = scene();
        let (f, cloud) = with_cloud();
        request_all_cloud(&f, &before, 1, id);
        assert_eq!(cloud.sent.lock().unwrap().len(), FRAMES.len(), "every frame downloaded once");
        assert!(FRAMES.iter().all(|fr| f.is_cached(&key(&f, &before, 1, id, *fr))));
        let after = edit(&before, id, 1, 10.);
        cloud.sent.lock().unwrap().clear();
        request_all_cloud(&f, &after, 2, id);
        let mut again: Vec<i64> = cloud.sent.lock().unwrap().iter().map(|(fr, _)| *fr).collect();
        again.sort_unstable();
        assert_eq!(again, vec![10, 20, 30, 40, 50], "only the frames between the edited key's neighbours");
        for fr in FRAMES {
            let expected = to_color_image(&effectcraft_engine::render::render_frame(&after, id, at(fr), 1.));
            assert_eq!(pixels(&f, &key(&f, &after, 2, id, fr)), expected.pixels, "frame {fr} equals a fresh render");
        }
        // Undo: nothing downloads, every frame of the first state is still here.
        cloud.sent.lock().unwrap().clear();
        request_all_cloud(&f, &before, 3, id);
        assert!(cloud.sent.lock().unwrap().is_empty(), "undo downloads nothing");
    }

    /// The disk keys the cloud server caches frames under follow what each frame shows: an edit
    /// keeps the key of every frame it doesn't change (the server's cache, which outlives the
    /// app, serves them) and changes the others'.
    #[test]
    fn server_disk_keys_change_only_for_the_frames_an_edit_changes() {
        let (before, id) = scene();
        let (f, cloud) = with_cloud();
        request_all_cloud(&f, &before, 1, id);
        let first: HashMap<i64, Option<u128>> = cloud.sent.lock().unwrap().iter().copied().collect();
        assert!(first.values().all(Option::is_some), "every frame has a disk key");
        // A fresh app (nothing in RAM) at the edited state: the server is asked for every frame.
        let after = edit(&before, id, 1, 10.);
        let (g, cloud2) = with_cloud();
        request_all_cloud(&g, &after, 7, id);
        let second: HashMap<i64, Option<u128>> = cloud2.sent.lock().unwrap().iter().copied().collect();
        let same: Vec<i64> = {
            let mut v: Vec<i64> = FRAMES.into_iter().filter(|fr| first[fr] == second[fr]).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(same, vec![0, 60, 70, 80, 90, 100, 110, 119], "the frames outside 0–2 s keep their keys");
    }

    /// The local disk cache names frames by their fingerprints too: after a restart (a fresh
    /// cache in RAM) at an edited state, the frames the edit didn't change come from disk (the
    /// blue bar shows them before they're asked for), the others don't, and every frame shown
    /// equals a fresh render.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_disk_cache_keeps_the_frames_an_edit_doesnt_change_across_a_restart() {
        use effectcraft_engine::render::disk_cache::DiskCache;
        let (before, id) = scene();
        let dir = std::env::temp_dir().join(format!("effectcraft-print-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        let disk_src = |p: &Arc<Project>| RenderSource { disk: Some(dc.clone()), ..source(p) };
        let opts = RenderOpts::default();
        let f = Frames::default();
        for frame in FRAMES {
            assert!(f.request_to_disk(&disk_src(&before), key(&f, &before, 1, id, frame), id, at(frame), opts));
        }
        wait(|| f.inflight() == 0);
        dc.flush();
        let after = edit(&before, id, 1, 10.);
        let g = Frames::default();
        let base = key(&g, &after, 9, id, 0);
        let mut blue = vec![];
        for _ in 0..400 {
            blue = g.disk_frames(&disk_src(&after), &base, 120, &opts);
            if !blue.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(blue, vec![0, 60, 70, 80, 90, 100, 110, 119], "on disk: the frames outside 0–2 s");
        for frame in FRAMES {
            g.request(&disk_src(&after), key(&g, &after, 9, id, frame), id, at(frame), opts);
        }
        wait(|| g.inflight() == 0);
        for fr in FRAMES {
            let expected = to_color_image(&effectcraft_engine::render::render_frame(&after, id, at(fr), 1.));
            assert_eq!(pixels(&g, &key(&g, &after, 9, id, fr)), expected.pixels, "frame {fr} equals a fresh render");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measure on a real project (`EC_PRINTS_PROJECT=…/project.ecproj`): for each layer with
    /// keyframes in the largest top-level comp, edit its middle keyframe and count the frames whose
    /// fingerprint the edit keeps (before this, every frame of the comp re-rendered, except the
    /// single-transform-property edits RT5 proved).
    #[test]
    #[ignore = "needs a local project file; prints counts"]
    fn real_project_frames_kept_after_one_keyframe_edit() {
        use effectcraft_engine::project::{ItemKind, LayerSource, Node, PropGroup};
        let Ok(path) = std::env::var("EC_PRINTS_PROJECT") else { return };
        let project = Arc::new(Project::from_json(&std::fs::read_to_string(path).unwrap()).unwrap());
        let nested: std::collections::HashSet<ItemId> = project
            .items
            .values()
            .filter_map(|i| if let ItemKind::Comp(c) = &i.kind { Some(c) } else { None })
            .flat_map(|c| c.layers.iter().filter_map(|l| if let LayerSource::Comp { item } = l.source { Some(item) } else { None }))
            .collect();
        let (id, comp) = project
            .items
            .iter()
            .filter_map(|(id, i)| if let ItemKind::Comp(c) = &i.kind { Some((*id, c.clone())) } else { None })
            .filter(|(id, _)| !nested.contains(id))
            .max_by_key(|(_, c)| c.layers.len())
            .unwrap();
        let src = source(&project);
        let fps = comp.frame_rate;
        // 3000 frames spread over the whole comp.
        let total = fps.frame_at(comp.duration).max(1);
        let n = total.min(3000);
        let frame = |i: i64| i * total / n;
        let t0 = web_time::Instant::now();
        let before: Vec<Option<u64>> = (0..n).map(|i| of(&src, id, fps.tick_of(frame(i)), RenderOpts::default())).collect();
        let per_frame = t0.elapsed().as_secs_f64() * 1000. / n as f64;
        eprintln!(
            "comp {} layers {} frames {n}; fingerprint {per_frame:.3} ms/frame; none {}",
            comp.layers.len(),
            comp.layers.len(),
            before.iter().filter(|p| p.is_none()).count()
        );
        fn first_keyed(g: &mut PropGroup) -> Option<&mut effectcraft_engine::project::Property> {
            for c in &mut g.children {
                match c {
                    Node::Prop(p) if p.keys.len() >= 3 && matches!(p.keys[p.keys.len() / 2].value, Value::Scalar(_) | Value::Vec2(_) | Value::Vec3(_)) => {
                        return Some(p);
                    }
                    Node::Prop(_) => {}
                    Node::Group(sub) => {
                        if let Some(p) = first_keyed(sub) {
                            return Some(p);
                        }
                    }
                }
            }
            None
        }
        let (mut edits, mut kept_total, mut rt5, mut excess_total) = (0usize, 0usize, 0usize, 0usize);
        for i in 0..comp.layers.len() {
            let mut after = project.clone();
            let c = Arc::make_mut(&mut after).comp_mut(id).unwrap();
            let name = c.layers.get(i).unwrap().name.clone();
            let Some(p) = first_keyed(&mut c.layers.get_mut(i).unwrap().props) else { continue };
            let k = p.keys.len() / 2;
            // After Effects re-renders the frames between the edited key's neighbours.
            let span = (p.keys[k - 1].time, p.keys[k + 1].time);
            p.keys[k].value = match &p.keys[k].value {
                Value::Scalar(v) => Value::Scalar(v * 0.5 + 1.),
                Value::Vec2(v) => Value::Vec2([v[0] + 7., v[1] - 3.]),
                Value::Vec3(v) => Value::Vec3([v[0] + 7., v[1] - 3., v[2]]),
                v => v.clone(),
            };
            let path = p.match_id.clone();
            let src = source(&after);
            let layer = after.comp(id).unwrap().layers.get(i).unwrap().clone();
            let (a, b) = (layer.comp_time(span.0), layer.comp_time(span.1));
            let (mut kept, mut excess) = (0, 0);
            for i in 0..n {
                let t = fps.tick_of(frame(i));
                if before[i as usize].is_some() && of(&src, id, t, RenderOpts::default()) == before[i as usize] {
                    kept += 1;
                } else if t < a.min(b) || t > a.max(b) {
                    excess += 1;
                }
            }
            let proof = super::super::property_reuse::Proof::between(&project, &after, id).is_some();
            eprintln!("layer {i} {name:?} {path}: kept {kept}/{n} frames, {excess} re-rendered outside the key's neighbours (RT5 proof: {proof})");
            edits += 1;
            kept_total += kept;
            excess_total += excess;
            rt5 += usize::from(proof);
        }
        let all = (edits.max(1) * n as usize) as f64;
        eprintln!(
            "{edits} keyframe edits: {:.1} % of frames kept on average, {:.1} % re-rendered outside the edited key's neighbours (before: 0 % kept, RT5 proved {rt5} edits)",
            100. * kept_total as f64 / all,
            100. * excess_total as f64 / all
        );
    }

    /// Frames a typical small edit invalidates in a real comp (`EC_PRINTS_PROJECT=…`, comp
    /// `EC_PRINTS_COMP`, default the largest top-level one): by the comp-wide content key (the
    /// disk cache's and the cloud server's frame keys before) and by the frame fingerprint
    /// (every cache now). Edits: a text layer nudged 12 px (the shortest-lived in the comp, then
    /// one inside a precomp) and the middle position keyframe of a keyed layer moved.
    #[test]
    #[ignore = "needs a local project file; prints counts"]
    fn real_project_small_edit_invalidation() {
        use effectcraft_engine::project::{ItemKind, LayerSource};
        use effectcraft_engine::render::disk_cache;
        let Ok(path) = std::env::var("EC_PRINTS_PROJECT") else { return };
        let project = Arc::new(Project::from_json(&std::fs::read_to_string(path).unwrap()).unwrap());
        let want = std::env::var("EC_PRINTS_COMP").ok();
        let (id, comp) = project
            .items
            .iter()
            .filter_map(|(id, i)| if let ItemKind::Comp(c) = &i.kind { Some((*id, i.name.clone(), c.clone())) } else { None })
            .filter(|(_, name, _)| want.as_ref().is_none_or(|w| w == name))
            .max_by_key(|(_, _, c)| c.layers.len())
            .map(|(id, _, c)| (id, c))
            .unwrap();
        let fps = comp.frame_rate;
        let total = fps.frame_at(comp.duration).max(1);
        let n = total.min(std::env::var("EC_PRINTS_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(1500));
        let frame = |i: i64| i * total / n;
        let src = source(&project);
        let t0 = web_time::Instant::now();
        let before: Vec<Option<u64>> = (0..n).map(|i| of(&src, id, fps.tick_of(frame(i)), RenderOpts::default())).collect();
        eprintln!(
            "comp {} layers, {total} frames ({n} sampled): fingerprint {:.3} ms/frame, {} without one",
            comp.layers.len(),
            t0.elapsed().as_secs_f64() * 1000. / n as f64,
            before.iter().filter(|p| p.is_none()).count()
        );
        let nudge = |p: &mut effectcraft_engine::project::Property, dx: f64| {
            let f = |v: &Value| match v {
                Value::Vec2(v) => Value::Vec2([v[0] + dx, v[1]]),
                Value::Vec3(v) => Value::Vec3([v[0] + dx, v[1], v[2]]),
                v => v.clone(),
            };
            if p.keys.is_empty() {
                p.value = f(&p.value);
            } else {
                p.keys.iter_mut().for_each(|k| k.value = f(&k.value));
            }
        };
        let report = |label: &str, after: &Arc<Project>, span: Option<(Tick, Tick)>| {
            let src = source(after);
            let (mut kept, mut outside) = (0, 0);
            for i in 0..n {
                let t = fps.tick_of(frame(i));
                if before[i as usize].is_some() && of(&src, id, t, RenderOpts::default()) == before[i as usize] {
                    kept += 1;
                } else if span.is_some_and(|(a, b)| t < a || t > b) {
                    outside += 1;
                }
            }
            let comp_key_kept = disk_cache::comp_content_key(&project, id) == disk_cache::comp_content_key(after, id);
            let span = span.map(|(a, b)| format!("{:.2}–{:.2} s", a.seconds(), b.seconds())).unwrap_or_default();
            eprintln!(
                "{label} [{span}]: comp-wide key invalidates {}/{n}; fingerprint invalidates {}/{n} ({outside} outside the span)",
                if comp_key_kept { 0 } else { n },
                n - kept
            );
        };
        // 1. The shortest-lived text layer of the comp, nudged.
        if let Some(i) = (0..comp.layers.len()).filter(|i| matches!(comp.layers.get(*i).unwrap().source, LayerSource::Text)).min_by_key(|i| {
            let l = comp.layers.get(*i).unwrap();
            l.out_point.0 - l.in_point.0
        }) {
            let mut after = project.clone();
            let l = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(i).unwrap();
            nudge(l.props.prop_mut("transform/position").unwrap(), 12.);
            let span = (l.in_point, l.out_point);
            let name = l.name.clone();
            report(&format!("text layer {name:?} moved 12 px"), &after, Some(span));
        }
        // 2. A text layer inside a precomp the comp uses (its precomp layer's span).
        'nested: for l in comp.layers.iter() {
            let LayerSource::Comp { item } = l.source else { continue };
            let Some(inner) = project.comp(item) else { continue };
            for j in 0..inner.layers.len() {
                if !matches!(inner.layers.get(j).unwrap().source, LayerSource::Text) {
                    continue;
                }
                let mut after = project.clone();
                let tl = Arc::make_mut(&mut after).comp_mut(item).unwrap().layers.get_mut(j).unwrap();
                nudge(tl.props.prop_mut("transform/position").unwrap(), 12.);
                let name = format!("{:?} in {:?}", tl.name, project.item(item).map(|i| i.name.clone()).unwrap_or_default());
                // Every use of the precomp in the comp shows it.
                let uses: Vec<(Tick, Tick)> =
                    comp.layers.iter().filter(|k| matches!(k.source, LayerSource::Comp { item: x } if x == item)).map(|k| (k.in_point, k.out_point)).collect();
                let span = uses.iter().fold(None, |acc: Option<(Tick, Tick)>, (a, b)| Some(acc.map_or((*a, *b), |(x, y)| (x.min(*a), y.max(*b)))));
                report(&format!("text layer {name} moved 12 px ({} uses)", uses.len()), &after, span);
                break 'nested;
            }
        }
        // 3. The middle position keyframe of the first layer with 3 or more.
        if let Some(i) = (0..comp.layers.len()).find(|i| comp.layers.get(*i).unwrap().props.prop("transform/position").is_some_and(|p| p.keys.len() >= 3)) {
            let mut after = project.clone();
            let l = Arc::make_mut(&mut after).comp_mut(id).unwrap().layers.get_mut(i).unwrap();
            let p = l.props.prop_mut("transform/position").unwrap();
            let k = p.keys.len() / 2;
            let span = (p.keys[k - 1].time, p.keys[k + 1].time);
            p.keys[k].value = match &p.keys[k].value {
                Value::Vec2(v) => Value::Vec2([v[0] + 7., v[1] - 3.]),
                Value::Vec3(v) => Value::Vec3([v[0] + 7., v[1] - 3., v[2]]),
                v => v.clone(),
            };
            let l = after.comp(id).unwrap().layers.get(i).unwrap();
            let name = l.name.clone();
            report(&format!("position keyframe of {name:?} moved"), &after, Some((l.comp_time(span.0), l.comp_time(span.1))));
        }
    }
}
