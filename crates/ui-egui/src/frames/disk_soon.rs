//! Viewer frames worth keeping on disk wait for a quiet moment (Q1, 1 Nov 2026).
//!
//! Every viewer frame that took [`super::DISK_MIN_RENDER`] or more used to go to the disk cache
//! the moment it was shown: a GPU frame was read back (8 MB at 1080p), then LZ4-compressed and
//! queued for writing. While scrubbing Elias's real comp that compression alone was 16–23 % of
//! all the app's CPU samples (profile of Tapa 26-10 C0666-001, Compress RAM Preview on), on
//! frames he had already moved past. Now such a frame is marked [`super::DiskPlan::soon`] and
//! kept in RAM as it is. This module's thread writes the marked frames once the viewer has asked
//! for nothing for [`QUIET`] and no playback or prefetch frame waits ([`super::Worker::quiet`]),
//! a few at a time, at background priority, through the same paths as before (the codec's
//! compression, the GPU's read-back, the disk cache's writer). A marked frame that leaves RAM
//! first goes to disk on its way out ([`super::Cache::spill`]; a GPU texture is read back for it
//! then), so it reaches the disk either way.
#![cfg(not(target_arch = "wasm32"))]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use effectcraft_engine::render::disk_cache::{self, DiskCache};
use effectcraft_gpu::{DisplayFrame, Gpu};

use super::{Cache, ColorBytes, FrameImage, FrameKey, Worker};

/// The viewer asked for no frame for this long: the user stopped scrubbing.
pub(super) const QUIET: Duration = Duration::from_millis(1000);

/// Past this since the viewer's last frame, marked frames are written even while playback or
/// prefetch frames are queued (never while a viewer frame is): a backlog that never empties
/// doesn't keep them off the disk.
pub(super) const MAX_WAIT: Duration = Duration::from_secs(10);

/// How often the thread looks whether it may write.
const POLL: Duration = Duration::from_millis(200);

/// Marked frames handed on per look, so a viewer frame asked for meanwhile waits for at most
/// that many hand-offs (each only queues work for other threads).
const BATCH: usize = 4;

/// GPU frames that left RAM marked and wait for their read-back at most; past it one is dropped
/// (not written, counted as a dropped spill).
pub(super) const MAX_READBACKS: usize = 8;

#[derive(Default)]
pub(super) struct DiskSoon {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// When the viewer last asked for a frame.
    last_urgent: Option<Instant>,
    /// The thread runs.
    running: bool,
    /// Bumped by every [`DiskSoon::wake`]: the thread exits only when nothing woke it since it
    /// last found no marked frame.
    wakes: u64,
    /// The compositor GPU frames are read back with (dropped when the thread exits or the GPU
    /// is retired, so a failed device is never kept alive by it).
    gpu: Option<Gpu>,
}

/// What one marked frame needs once the cache lock is released.
enum Then {
    Nothing,
    /// Read back and compress (Compress RAM Preview on): the codec writes it from its bytes.
    Pack(FrameKey, Arc<DisplayFrame>),
    /// Read back for the disk cache's writer (compression off).
    ReadBack(Arc<DisplayFrame>, Arc<DiskCache>, u128),
}

impl DiskSoon {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The viewer asked for a frame (scrubbing): marked frames wait.
    pub(super) fn viewer_asked(&self) {
        self.lock().last_urgent = Some(Instant::now());
    }

    /// How long ago the viewer last asked for a frame (`None`: never).
    fn since_viewer(&self, now: Instant) -> Option<Duration> {
        self.lock().last_urgent.map(|t| now.saturating_duration_since(t))
    }

    /// Frames were marked (or a marked GPU frame left RAM): start the thread unless it runs.
    /// `gpu`: the compositor a marked GPU frame is read back with.
    pub(super) fn wake(&self, worker: &Worker, gpu: Option<&Gpu>) {
        let mut s = self.lock();
        s.wakes = s.wakes.wrapping_add(1);
        if let Some(gpu) = gpu {
            s.gpu = Some(gpu.clone());
        }
        if s.running {
            return;
        }
        s.running = true;
        let w = worker.clone();
        let spawned = std::thread::Builder::new().name("ec-disk-soon".into()).spawn(move || {
            // Background priority, as the codec's threads.
            super::idle::thread_start();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| w.disk_soon_run())).is_err() {
                log::error!("the thread writing viewer frames to the disk cache panicked; they go to disk when they leave RAM");
                let mut s = w.disk_soon.lock();
                s.running = false;
                s.gpu = None;
            }
        });
        if let Err(e) = spawned {
            // The frames stay marked: they go to disk when they leave RAM, or at the next wake.
            log::warn!("cannot start the thread writing viewer frames to the disk cache: {e}");
            s.running = false;
            s.gpu = None;
        }
    }
}

impl Worker {
    /// The viewer asked for nothing for [`QUIET`] and none of its frames is queued or rendering;
    /// nor is a playback or prefetch frame, unless the viewer's last frame is [`MAX_WAIT`] ago.
    /// Idle frames (Cache Frames When Idle, disk only) don't count: they run at background
    /// priority themselves.
    pub(super) fn quiet(&self, now: Instant) -> bool {
        let since = self.disk_soon.since_viewer(now);
        if since.is_some_and(|d| d < QUIET) {
            return false;
        }
        let Ok(q) = self.queue.lock() else { return false };
        if q.urgent_running > 0 || q.jobs.iter().any(|j| j.urgent) {
            return false;
        }
        let backlog = q.jobs.iter().any(|j| !j.idle && !j.disk_only);
        !backlog || since.is_none_or(|d| d >= MAX_WAIT)
    }

    /// The thread: while marked frames are left, hand [`BATCH`] of them on whenever it is quiet.
    fn disk_soon_run(&self) {
        loop {
            std::thread::sleep(POLL);
            let gpu = {
                let mut s = self.disk_soon.lock();
                if self.gpu_retired.load(std::sync::atomic::Ordering::Acquire) {
                    s.gpu = None;
                }
                s.gpu.clone()
            };
            let wakes = self.disk_soon.lock().wakes;
            // The viewer is gone (only this thread still holds its cache): nobody scrubs it, so
            // everything marked goes now and the thread ends.
            let orphaned = Arc::strong_count(&self.cache) <= 1;
            let left = if orphaned {
                self.flush_soon(gpu.as_ref(), usize::MAX, true)
            } else if self.quiet(Instant::now()) {
                self.flush_soon(gpu.as_ref(), BATCH, false)
            } else {
                self.soon_left()
            };
            drop(gpu);
            if left == 0 || orphaned {
                let mut s = self.disk_soon.lock();
                if s.wakes == wakes {
                    s.running = false;
                    s.gpu = None;
                    return;
                }
            }
        }
    }

    /// Marked frames still in RAM, and marked GPU frames waiting for their read-back.
    fn soon_left(&self) -> usize {
        let Ok(c) = self.cache.lock() else { return 0 };
        c.map.values().filter(|s| s.disk.soon).count().saturating_add(c.soon_readbacks.len())
    }

    /// Hand at most `batch` marked frames (least recently shown first) to the disk: their
    /// compressed bytes as they are, a CPU frame through the codec (compression on) or the disk
    /// cache's writer, a GPU frame through a read-back. Also starts the read-backs of marked
    /// GPU frames that left RAM. `direct`: the disk cache's writer compresses them, not the
    /// codec (whose result would come back to a cache that is going away). Returns how many
    /// marked frames are left.
    pub(super) fn flush_soon(&self, gpu: Option<&Gpu>, batch: usize, direct: bool) -> usize {
        let mut then = Vec::new();
        let readbacks;
        let left = {
            let Ok(mut c) = self.cache.lock() else { return 0 };
            readbacks = std::mem::take(&mut c.soon_readbacks);
            let keys: Vec<FrameKey> = c.order.iter().filter(|k| c.map.get(k).is_some_and(|s| s.disk.soon)).take(batch).copied().collect();
            for k in keys {
                match c.write_soon(k, direct) {
                    Some(t) => then.push(t),
                    // The writer's queue is full: the rest waits for the next look.
                    None => break,
                }
            }
            c.map.values().filter(|s| s.disk.soon).count()
        };
        for (f, dc, dk) in readbacks {
            if let Some(gpu) = gpu {
                self.read_back_for_disk(gpu, &f, dc, dk);
            }
        }
        for t in then {
            match (t, gpu) {
                (Then::Pack(k, f), Some(gpu)) => self.pack_gpu(gpu, k, &f),
                (Then::ReadBack(f, dc, dk), Some(gpu)) => self.read_back_for_disk(gpu, &f, dc, dk),
                _ => {}
            }
        }
        left
    }

    /// Start the read-backs of marked GPU frames that left RAM ([`Cache::spill`]).
    pub(super) fn soon_readbacks(&self, gpu: &Gpu) {
        let readbacks = match self.cache.lock() {
            Ok(mut c) if !c.soon_readbacks.is_empty() => std::mem::take(&mut c.soon_readbacks),
            _ => return,
        };
        for (f, dc, dk) in readbacks {
            self.read_back_for_disk(gpu, &f, dc, dk);
        }
    }
}

impl Cache {
    /// Frame `k` is marked: send it on its way to disk now (see [`Worker::flush_soon`]).
    /// `None` when the disk cache's write queue has no room for it (it stays marked). `direct`:
    /// not through the codec (see [`Worker::flush_soon`]).
    fn write_soon(&mut self, k: FrameKey, direct: bool) -> Option<Then> {
        let compress = self.compress && !direct;
        let slot = self.map.get_mut(&k)?;
        let Some((dc, dk)) = slot.disk.target.clone().filter(|_| slot.disk.soon) else {
            slot.disk.soon = false;
            return Some(Then::Nothing);
        };
        if dc.contains(disk_cache::Kind::Frame, dk) {
            slot.disk = super::DiskPlan::default();
            return Some(Then::Nothing);
        }
        let bytes = slot.raw.as_ref().map_or(0, FrameImage::bytes);
        if !dc.has_room(bytes) {
            return None;
        }
        slot.disk.soon = false;
        match (&slot.packed, &slot.raw) {
            (Some(p), _) => {
                // Queued: nothing left to spill. Not queued: it may still spill later.
                if dc.put_packed_frame(dk, p.clone()) {
                    slot.disk.target = None;
                }
                Some(Then::Nothing)
            }
            (None, Some(FrameImage::Cpu(img))) => {
                if compress {
                    // As a frame written at once used to be: compressed now on the codec's
                    // threads, written from those bytes ([`Cache::attach_packed`]).
                    slot.disk.now = true;
                    self.pack(k);
                } else {
                    dc.put_frame(dk, img.size[0] as u32, img.size[1] as u32, ColorBytes(img.clone()));
                    slot.disk.target = None;
                }
                Some(Then::Nothing)
            }
            (None, Some(FrameImage::Gpu(f))) => {
                let f = f.clone();
                if compress {
                    slot.disk.now = true;
                    Some(Then::Pack(k, f))
                } else {
                    slot.disk.target = None;
                    Some(Then::ReadBack(f, dc, dk))
                }
            }
            (None, None) => Some(Then::Nothing),
        }
    }
}
