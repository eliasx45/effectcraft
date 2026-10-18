//! Frame disk keys named by what a frame shows: a frame whose fingerprint
//! ([`super::fingerprint`]) is known is stored on disk and in the cloud server's frame cache
//! under its fingerprint, salted with the footage files' identities (the fingerprint names
//! footage items, not file contents), the comp, frame, scale, view and render options. An edit
//! then changes the disk keys of only the frames it changes, as it does in RAM, and the frames on
//! disk or on the server survive edits elsewhere in the comp and restarts. A frame without a
//! fingerprint keeps the comp-wide content key ([`super::frame_disk_key`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use effectcraft_engine::project::Project;
use effectcraft_engine::render::disk_cache;

use super::FrameKey;

/// Footage salts by project revision.
pub(super) type Salts = Arc<Mutex<HashMap<u64, u64>>>;

/// The footage salt of `project` at `revision` (computed once per revision; off the UI thread).
pub(super) fn salt(salts: &Salts, project: &Project, revision: u64) -> u64 {
    if let Some(s) = salts.lock().unwrap_or_else(PoisonError::into_inner).get(&revision).copied() {
        return s;
    }
    let s = disk_cache::footage_salt(project);
    let mut m = salts.lock().unwrap_or_else(PoisonError::into_inner);
    if m.len() > 64 {
        m.clear();
    }
    m.insert(revision, s);
    s
}

/// Disk key of the frame `key` whose fingerprint is `print` (`opts_hash`: its render options').
pub(super) fn print_disk_key(salts: &Salts, project: &Project, key: &FrameKey, print: u64, opts_hash: u64) -> u128 {
    let content = disk_cache::print_content(print, salt(salts, project, key.revision), key.comp);
    disk_cache::frame_key(content, key.frame, key.scale, key.view ^ opts_hash)
}

/// The fingerprints of every frame of a series (a comp at a revision with render options): what
/// names its frames on disk and on the cloud server without rendering them (the blue cache bar,
/// Cache Frames When Idle). `None` for a frame without a fingerprint (its comp-wide key names it).
pub struct PrintTable {
    pub prints: effectcraft_engine::remote::FramePrints,
}

impl PrintTable {
    /// Disk key of frame `key.frame` of the table's series, when it has a fingerprint.
    pub(super) fn disk_key(&self, key: &FrameKey, opts_hash: u64) -> Option<u128> {
        let print = (*self.prints.prints.get(usize::try_from(key.frame).ok()?)?)?;
        let content = disk_cache::print_content(print, self.prints.salt, key.comp);
        Some(disk_cache::frame_key(content, key.frame, key.scale, key.view ^ opts_hash))
    }
}

/// A series' table: (revision, comp, scale, view, options hash).
pub(super) type TableKey = (u64, u64, u32, u64, u64);

pub(super) fn table_key(key: &FrameKey, opts_hash: u64) -> TableKey {
    (key.revision, key.comp, key.scale, key.view, opts_hash)
}

/// Tables kept (the newest).
const KEPT_TABLES: usize = 6;

/// Frames a table covers, at most.
pub(super) const MAX_TABLE_FRAMES: i64 = 100_000;

/// A table to compute.
#[cfg(not(target_arch = "wasm32"))]
struct TableJob {
    key: TableKey,
    src: super::RenderSource,
    comp: effectcraft_engine::project::ItemId,
    opts: effectcraft_engine::render::RenderOpts,
}

/// Print tables, computed on the frame pool (never the UI thread): one at a time, the newest
/// request first; a request for a newer revision of the same comp stops an older one.
#[derive(Default)]
pub(super) struct Tables {
    done: Mutex<std::collections::VecDeque<(TableKey, Arc<PrintTable>)>>,
    #[cfg(not(target_arch = "wasm32"))]
    pending: Mutex<(bool, Option<TableJob>)>,
    /// Bumped by every request: a running table for an older revision of its comp stops.
    #[cfg(not(target_arch = "wasm32"))]
    newest: Mutex<HashMap<u64, u64>>,
}

impl Tables {
    pub(super) fn get(&self, key: &TableKey) -> Option<Arc<PrintTable>> {
        self.done.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(k, _)| k == key).map(|(_, t)| t.clone())
    }

    /// The table of `key`, or `None` while a pool thread computes it (`ctx` repaints when it's
    /// ready).
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn get_or_request(
        self: &Arc<Self>,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>),
        salts: &Salts,
        ctx: Option<&egui::Context>,
        src: &super::RenderSource,
        key: TableKey,
        comp: effectcraft_engine::project::ItemId,
        opts: effectcraft_engine::render::RenderOpts,
    ) -> Option<Arc<PrintTable>> {
        if let Some(t) = self.get(&key) {
            return Some(t);
        }
        {
            let mut newest = self.newest.lock().unwrap_or_else(PoisonError::into_inner);
            let n = newest.entry(key.1).or_default();
            *n = (*n).max(key.0);
        }
        let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if p.1.as_ref().is_some_and(|j| j.key == key) {
            return None;
        }
        p.1 = Some(TableJob { key, src: src.clone(), comp, opts });
        if p.0 {
            return None;
        }
        p.0 = true;
        drop(p);
        let (tables, salts, ctx) = (self.clone(), salts.clone(), ctx.cloned());
        spawn(Box::new(move || {
            loop {
                let job = {
                    let mut p = tables.pending.lock().unwrap_or_else(PoisonError::into_inner);
                    let job = p.1.take();
                    if job.is_none() {
                        p.0 = false;
                    }
                    job
                };
                let Some(job) = job else { return };
                if tables.get(&job.key).is_some() {
                    continue;
                }
                if let Some(t) = tables.compute(&job, &salts) {
                    let mut done = tables.done.lock().unwrap_or_else(PoisonError::into_inner);
                    done.retain(|(k, _)| *k != job.key);
                    if done.len() >= KEPT_TABLES {
                        done.pop_front();
                    }
                    done.push_back((job.key, Arc::new(t)));
                    drop(done);
                    if let Some(ctx) = &ctx {
                        ctx.request_repaint();
                    }
                }
            }
        }));
        None
    }

    /// The fingerprints of every frame of `job`'s comp, in parallel; `None` when a newer revision
    /// of the comp was asked for meanwhile.
    #[cfg(not(target_arch = "wasm32"))]
    fn compute(&self, job: &TableJob, salts: &Salts) -> Option<PrintTable> {
        use rayon::prelude::*;
        // A comp that isn't there has no frames (an empty table: no request again).
        let Some(c) = job.src.project.comp(job.comp) else { return Some(PrintTable { prints: Default::default() }) };
        let (fps, frames) = (c.frame_rate, c.frame_rate.frame_at(c.duration).clamp(0, MAX_TABLE_FRAMES));
        let stale = || self.newest.lock().unwrap_or_else(PoisonError::into_inner).get(&job.key.1).is_some_and(|n| *n > job.key.0);
        let mut prints = Vec::with_capacity(usize::try_from(frames).unwrap_or(0));
        for chunk in (0..frames).collect::<Vec<_>>().chunks(256) {
            if stale() {
                return None;
            }
            let part: Vec<Option<u64>> = chunk.par_iter().map(|f| super::fingerprint::of(&job.src, job.comp, fps.tick_of(*f), job.opts)).collect();
            prints.extend(part);
        }
        let salt = salt(salts, &job.src.project, job.key.0);
        Some(PrintTable { prints: effectcraft_engine::remote::FramePrints { salt, prints } })
    }
}
