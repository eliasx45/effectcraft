//! The cloud frame server's own frame cache (`plan/cloud-render.md` CR8): finished viewer frames
//! kept on the server, so the app's Cache Frames When Idle renders there and only the frames the
//! editor looks at cross the internet.
//!
//! - Keys are the app's frame keys ([`effectcraft_engine::remote::CacheSeries::key`], the disk
//!   cache's [`disk_cache::frame_key`]): the app names a frame in a render request
//!   ([`effectcraft_engine::remote::FrameMsg::Render`] `disk`) or a precache plan, and the server
//!   never makes keys of its own. An edit changes the comp's content key, so frames from before
//!   are never served for the new content; they age out (least recently used first).
//! - RAM first (full-size premultiplied RGBA8, up to its byte budget); frames pushed out of RAM go
//!   to an optional disk cache and come back from it on a hit.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use effectcraft_engine::render::disk_cache::{DiskCache, Kind};

use crate::control_wire::lock;

/// A cached frame: full-size premultiplied RGBA8.
#[derive(Clone)]
pub struct CachedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

struct Entry {
    frame: CachedFrame,
    stamp: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<u128, Entry>,
    /// Least recently used first.
    lru: BTreeMap<u64, u128>,
    stamp: u64,
    bytes: usize,
    hits: u64,
    misses: u64,
}

/// What the cache holds (the server log, [`effectcraft_engine::remote::FrameReply::Cached`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CacheStats {
    pub frames: u64,
    pub ram_bytes: u64,
    pub ram_budget: u64,
    pub disk_bytes: u64,
    pub hits: u64,
    pub misses: u64,
}

/// Shares an `Arc`'d frame with the disk writer without a copy.
struct Shared(Arc<Vec<u8>>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// See the module docs.
pub struct FrameCache {
    inner: Mutex<Inner>,
    budget: usize,
    disk: Option<Arc<DiskCache>>,
}

impl FrameCache {
    /// A cache of `budget` bytes in RAM, spilling to `disk` when there is one.
    pub fn new(budget: usize, disk: Option<Arc<DiskCache>>) -> FrameCache {
        FrameCache { inner: Mutex::new(Inner::default()), budget, disk }
    }

    /// The frame of `key`, from RAM or disk (a disk hit comes back into RAM).
    pub fn get(&self, key: u128) -> Option<CachedFrame> {
        {
            let mut g = lock(&self.inner);
            let inner = &mut *g;
            inner.stamp += 1;
            let stamp = inner.stamp;
            if let Some(e) = inner.map.get_mut(&key) {
                inner.lru.remove(&e.stamp);
                e.stamp = stamp;
                inner.lru.insert(stamp, key);
                inner.hits += 1;
                return Some(e.frame.clone());
            }
        }
        let found = self.disk.as_ref().filter(|d| d.contains(Kind::Frame, key)).and_then(|d| d.get_frame(key));
        let mut g = lock(&self.inner);
        match found {
            Some(f) => {
                g.hits += 1;
                drop(g);
                let frame = CachedFrame { width: f.width, height: f.height, rgba: Arc::new(f.rgba) };
                self.put_ram(key, frame.clone());
                Some(frame)
            }
            None => {
                g.misses += 1;
                None
            }
        }
    }

    /// Whether `key` is cached (RAM or disk), without touching it.
    pub fn contains(&self, key: u128) -> bool {
        lock(&self.inner).map.contains_key(&key) || self.disk.as_ref().is_some_and(|d| d.contains(Kind::Frame, key))
    }

    /// Keep a frame (full size). Frames over the budget leave the least recently used first.
    pub fn put(&self, key: u128, width: u32, height: u32, rgba: Arc<Vec<u8>>) {
        let n = (width as usize).checked_mul(height as usize).and_then(|p| p.checked_mul(4));
        if n != Some(rgba.len()) || rgba.is_empty() {
            return;
        }
        self.put_ram(key, CachedFrame { width, height, rgba });
    }

    fn put_ram(&self, key: u128, frame: CachedFrame) {
        let size = frame.rgba.len();
        if size > self.budget {
            if let Some(d) = &self.disk {
                d.put_frame(key, frame.width, frame.height, Shared(frame.rgba));
            }
            return;
        }
        let mut out = Vec::new();
        {
            let mut g = lock(&self.inner);
            let inner = &mut *g;
            inner.stamp += 1;
            let stamp = inner.stamp;
            if let Some(old) = inner.map.insert(key, Entry { frame, stamp }) {
                inner.lru.remove(&old.stamp);
                inner.bytes = inner.bytes.saturating_sub(old.frame.rgba.len());
            }
            inner.lru.insert(stamp, key);
            inner.bytes = inner.bytes.saturating_add(size);
            while inner.bytes > self.budget {
                let Some((_, victim)) = inner.lru.pop_first() else { break };
                if let Some(e) = inner.map.remove(&victim) {
                    inner.bytes = inner.bytes.saturating_sub(e.frame.rgba.len());
                    out.push((victim, e.frame));
                }
            }
        }
        if let Some(d) = &self.disk {
            for (k, f) in out {
                d.put_frame(k, f.width, f.height, Shared(f.rgba));
            }
        }
    }

    /// Drop the frames in RAM (Edit ▸ Purge); the disk keeps its own.
    pub fn clear(&self) {
        let mut g = lock(&self.inner);
        g.map.clear();
        g.lru.clear();
        g.bytes = 0;
    }

    pub fn stats(&self) -> CacheStats {
        let g = lock(&self.inner);
        CacheStats {
            frames: g.map.len() as u64,
            ram_bytes: g.bytes as u64,
            ram_budget: self.budget as u64,
            disk_bytes: self.disk.as_ref().map_or(0, |d| d.stats().bytes),
            hits: g.hits,
            misses: g.misses,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: u8) -> Arc<Vec<u8>> {
        Arc::new(vec![v; 2 * 2 * 4])
    }

    #[test]
    fn least_recently_used_frames_leave_first_and_bad_sizes_are_refused() {
        let c = FrameCache::new(16 * 2, None);
        c.put(1, 2, 2, frame(1));
        c.put(2, 2, 2, frame(2));
        assert!(c.get(1).is_some());
        c.put(3, 2, 2, frame(3));
        assert!(c.contains(1) && c.contains(3) && !c.contains(2));
        c.put(4, 3, 3, frame(4));
        assert!(!c.contains(4));
        assert_eq!(c.stats().frames, 2);
        assert_eq!(c.stats().ram_bytes, 32);
        c.clear();
        assert!(!c.contains(1));
    }

    #[test]
    fn frames_pushed_out_of_ram_come_back_from_disk() {
        let dir = std::env::temp_dir().join(format!("ec-frame-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = DiskCache::open(&dir, 1 << 30).unwrap();
        let c = FrameCache::new(16, Some(disk.clone()));
        c.put(1, 2, 2, frame(7));
        c.put(2, 2, 2, frame(8));
        disk.flush();
        assert!(c.contains(1), "the evicted frame is on disk");
        let back = c.get(1).unwrap();
        assert_eq!(back.rgba.as_slice(), &[7; 16]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
