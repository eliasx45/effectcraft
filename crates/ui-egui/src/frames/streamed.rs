//! Frames of the cloud's playback stream kept on this Mac (CR6c, `plan/cloud-render.md`): every
//! frame the stream brought stays in the RAM preview when playback stops, and goes into this
//! Mac's disk cache, so playing the same part again (or after a restart) plays from here instead
//! of rendering it on the server and downloading it again.
//!
//! - **Drafts.** A frame that came below the viewer's size (the stream lowers it while its lead
//!   is short), or one an edit made old while it played, is a *draft*: cached under the viewer's
//!   key and played, shown lighter on the cache bar, and never taken as exact: the paused viewer
//!   renders the frame on screen over it ([`Frames::request_exact`]), and any other frame stored
//!   under the key replaces it. A draft never replaces an exact frame.
//! - **Disk keys.** A streamed frame's disk key is its viewer frame's ([`super::frame_disk_key`])
//!   at the size it came at, with [`STREAM_TAG`] in the view hash: an H.264 frame of the stream
//!   is never found as a rendered frame, nor a draft as a full-size one.

use std::sync::{Arc, Mutex, PoisonError};

use effectcraft_engine::project::Project;
use effectcraft_engine::render::disk_cache::{self, DiskCache};

use super::{Cache, ContentKeys, FrameImage, FrameKey, Frames, frame_disk_key};

/// Mixed into the view hash of a streamed frame's disk key.
const STREAM_TAG: u64 = 0x5354_5245_414d_0c6c;
/// The sizes a stream's frames come at (relative to the viewer's), largest first: full, half,
/// quarter (`effectcraft_engine::remote_stream`'s adaptive resolution).
pub const STREAM_SCALES: [f32; 3] = [1.0, 0.5, 0.25];

/// Whether a stream frame at `scale` of the viewer's size is a draft (below full size).
pub fn is_draft_scale(scale: f32) -> bool {
    // NaN is a draft too.
    scale.is_nan() || scale < 0.99
}

/// The nearest of [`STREAM_SCALES`] (what a disk key holds).
fn snap_scale(scale: f32) -> f32 {
    STREAM_SCALES.iter().copied().min_by(|a, b| (a - scale).abs().total_cmp(&(b - scale).abs())).unwrap_or(1.0)
}

/// The disk key of `key`'s frame as the stream sent it at `scale`; `None` until the comp's
/// content key is known (never hashed on the calling thread, see [`Frames::streamed_disk_key`]).
fn disk_key(keys: &ContentKeys, project: &Project, key: &FrameKey, opts_hash: u64, scale: f32) -> u128 {
    let milli = ((f64::from(key.scale) * f64::from(snap_scale(scale))).round() as u32).max(1);
    frame_disk_key(keys, project, &FrameKey { scale: milli, view: key.view ^ STREAM_TAG, ..*key }, opts_hash)
}

/// Where frames read back from the disk cache go (a thread of its own fills the RAM preview
/// with them while playback reads it).
#[derive(Clone)]
pub struct StreamedSink {
    cache: Arc<Mutex<Cache>>,
    ctx: Option<egui::Context>,
}

impl StreamedSink {
    /// See [`Frames::insert_streamed`].
    pub fn insert(&self, k: FrameKey, img: Arc<egui::ColorImage>, draft: bool) {
        insert(&self.cache, k, img, draft);
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
    }
}

fn insert(cache: &Mutex<Cache>, k: FrameKey, img: Arc<egui::ColorImage>, draft: bool) {
    let mut c = cache.lock().unwrap_or_else(PoisonError::into_inner);
    if c.map.contains_key(&k) && !c.drafts.contains(&k) {
        // An exact frame (or a full-size streamed one) is there already: it stays.
        return;
    }
    c.insert(k, FrameImage::Cpu(img));
    if draft {
        c.drafts.insert(k);
    }
}

impl Frames {
    /// A frame of the cloud's playback stream (CR6b), cached under the viewer's key `k` as it
    /// came (a half- or quarter-size frame shows scaled up); `draft`: below full size, or made
    /// old by an edit. It replaces a draft, never another frame.
    pub fn insert_streamed(&self, k: FrameKey, img: Arc<egui::ColorImage>, draft: bool) {
        insert(&self.cache, k, img, draft);
    }

    /// The frame under `k` is a draft of the stream.
    pub fn is_draft(&self, k: &FrameKey) -> bool {
        self.cache.lock().is_ok_and(|c| c.drafts.contains(k))
    }

    /// A frame is cached under `k` and it is not a draft.
    pub fn is_exact(&self, k: &FrameKey) -> bool {
        self.cache.lock().is_ok_and(|c| c.map.contains_key(k) && !c.drafts.contains(k))
    }

    /// Draft frame numbers of the series of `base`, sorted (the cache bar's lighter green).
    pub fn draft_frames(&self, base: &FrameKey) -> Vec<i64> {
        let Ok(c) = self.cache.lock() else { return vec![] };
        let mut v: Vec<i64> = c.drafts.iter().filter(|k| k.same_series(base) && c.map.contains_key(k)).map(|k| k.frame).collect();
        v.sort_unstable();
        v
    }

    /// A handle that caches streamed frames from another thread.
    pub fn streamed_sink(&self) -> StreamedSink {
        StreamedSink { cache: self.cache.clone(), ctx: self.ctx.clone() }
    }

    /// Whether the content key of `key`'s comp at its revision is known; a worker computes it
    /// when it isn't (hashing a comp never runs on the UI thread).
    pub fn content_key_ready(&self, project: &Arc<Project>, key: &FrameKey) -> bool {
        let known = self.content_keys.lock().unwrap_or_else(PoisonError::into_inner).contains_key(&(key.revision, key.comp));
        if !known {
            self.request_content_key(project.clone(), key.revision, key.comp);
        }
        known
    }

    /// The disk key of `key`'s frame as the stream sent it at `scale` (`None` until its content
    /// key is ready, see [`Frames::content_key_ready`]).
    pub fn streamed_disk_key(&self, project: &Arc<Project>, key: &FrameKey, opts_hash: u64, scale: f32) -> Option<u128> {
        self.content_key_ready(project, key).then(|| disk_key(&self.content_keys, project, key, opts_hash, scale))
    }

    /// The best copy of `key`'s frame in the disk cache: the rendered frame, else the stream's
    /// largest. Its disk key and whether it is a draft.
    pub fn streamed_on_disk(&self, dc: &DiskCache, project: &Arc<Project>, key: &FrameKey, opts_hash: u64) -> Option<(u128, bool)> {
        if !self.content_key_ready(project, key) {
            return None;
        }
        let rendered = frame_disk_key(&self.content_keys, project, key, opts_hash);
        if dc.contains(disk_cache::Kind::Frame, rendered) {
            return Some((rendered, false));
        }
        STREAM_SCALES.iter().find_map(|s| {
            let k = disk_key(&self.content_keys, project, key, opts_hash, *s);
            dc.contains(disk_cache::Kind::Frame, k).then_some((k, is_draft_scale(*s)))
        })
    }
}

/// A frame read back from the disk cache, as the viewer shows it (`None`: damaged or gone).
pub fn read_back(dc: &DiskCache, key: u128) -> Option<Arc<egui::ColorImage>> {
    let f = dc.get_frame(key)?;
    let n = (f.width as usize).checked_mul(f.height as usize)?.checked_mul(4)?;
    (n == f.rgba.len() && n > 0).then(|| Arc::new(egui::ColorImage::from_rgba_premultiplied([f.width as usize, f.height as usize], &f.rgba)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(frame: i64) -> FrameKey {
        FrameKey { revision: 1, content: 7, comp: 3, frame, scale: 500, view: 0, opts: 9 }
    }

    fn img(w: usize) -> Arc<egui::ColorImage> {
        Arc::new(egui::ColorImage::new([w, 2], vec![egui::Color32::RED; w * 2]))
    }

    #[test]
    fn a_draft_gives_way_to_a_full_frame_and_never_replaces_one() {
        let f = Frames::default();
        f.insert_streamed(key(0), img(4), true);
        assert!(f.is_cached(&key(0)) && f.is_draft(&key(0)) && !f.is_exact(&key(0)));
        assert_eq!(f.draft_frames(&key(0)), vec![0]);
        // A full-size copy replaces it.
        f.insert_streamed(key(0), img(8), false);
        assert!(f.is_exact(&key(0)) && f.draft_frames(&key(0)).is_empty());
        // A later draft doesn't replace the full frame.
        f.insert_streamed(key(0), img(4), true);
        assert!(f.is_exact(&key(0)));
        assert_eq!(f.get(&key(0)).map(|i| i.size()[0]), Some(8));
        // Taking a draft out forgets it.
        f.insert_streamed(key(1), img(4), true);
        f.remove_frame(&key(1));
        assert!(!f.is_cached(&key(1)) && !f.is_draft(&key(1)));
    }

    #[test]
    fn stream_disk_keys_hold_the_size_and_differ_from_rendered_ones() {
        let keys = ContentKeys::default();
        let p = Project::default();
        keys.lock().unwrap().insert((1, 3), 42);
        let k = key(5);
        let full = disk_key(&keys, &p, &k, 9, 1.0);
        let half = disk_key(&keys, &p, &k, 9, 0.5);
        assert_ne!(full, half);
        assert_eq!(half, disk_key(&keys, &p, &k, 9, 0.52), "snapped to the stream's sizes");
        assert_ne!(full, frame_disk_key(&keys, &p, &k, 9), "a stream frame is never a rendered one");
        assert!(is_draft_scale(0.5) && is_draft_scale(f32::NAN) && !is_draft_scale(1.0));
    }
}
