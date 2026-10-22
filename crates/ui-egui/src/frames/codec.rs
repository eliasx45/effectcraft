//! The RAM preview's compressed frames (`plan/memory/compressed-cache.md`, Memory & CPU ▸
//! Compress RAM Preview Frames): a frame is compressed once, off the render path, into 16 LZ4
//! strips ([`PackedFrame`]); the cache then keeps the compressed bytes and drops the frame's
//! pixels. Its budget counts the compressed bytes, so the same memory holds about 2.4× the
//! frames (measured on real 1080p frames, `plan/memory/cache-prototypes.md`).
//!
//! Compression is lazy (MEM2): frames keep their pixels while the uncompressed share of the
//! budget has room, and only the frames about to be pushed out of it are compressed, a little
//! ahead of time (`Cache::trim_raw`). The threads run the app's thread-start hook first
//! ([`super::idle::set_thread_start`]: background priority), so they never take CPU from the
//! render workers.
//!
//! - **Pack.** A CPU frame is compressed from the pixels the cache shares (no copy); a GPU frame
//!   is read back first (the landing thread, `Worker::pack_gpu`). The compressed bytes are the
//!   disk cache's frame payload: a frame on its way to disk is written from them
//!   (`DiskCache::put_packed_frame`), never compressed twice.
//! - **Unpack.** Frames the playing preview reaches within Uncompressed Frames Around the
//!   Playhead are decoded ahead of it on these threads, so playback shows pixels at once; any
//!   other compressed frame the viewer asks for decodes on the spot, its strips in parallel
//!   (about 2 ms for 1080p).
//!
//! At most [`THREADS`] threads run, unpacking before packing; they exit while there is nothing
//! to do. The queues are bounded: a frame that doesn't fit stays uncompressed (and is offered
//! again later, `Cache::trim_raw`). Results come back by key and are dropped when the frame was
//! replaced or evicted meanwhile (`Cache::attach_packed`, `Cache::attach_raw`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use effectcraft_engine::render::frame_pack::PackedFrame;

use super::{Cache, FrameImage, FrameKey};

/// Codec threads at most.
pub(super) const THREADS: usize = 2;

/// Threads the viewer's own decode of a compressed frame uses (its strips in parallel).
pub(super) const VIEWER_DECODE_THREADS: usize = 4;

/// Frames waiting to be compressed at most.
const MAX_PACK_JOBS: usize = 64;

/// Bytes of read-back GPU frames waiting to be compressed at most (CPU frames share the cache's
/// pixels and hold nothing extra).
const MAX_PACK_BYTES: usize = 512 << 20;

/// Frames waiting to be decoded ahead of the playhead at most.
const MAX_UNPACK_JOBS: usize = 256;

/// A frame's pixels to compress.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) enum Pixels {
    /// A CPU frame, shared with the cache.
    Image(Arc<egui::ColorImage>),
    /// A GPU frame read back: width, height, premultiplied RGBA8.
    Bytes(u32, u32, Vec<u8>),
}

pub(super) struct PackJob {
    pub key: FrameKey,
    /// The pixels' identity in the cache ([`super::image_id`]): a frame replaced meanwhile keeps
    /// its new pixels.
    pub raw_id: usize,
    pub pixels: Pixels,
}

impl PackJob {
    /// Bytes it holds besides the cache's own.
    fn held(&self) -> usize {
        match &self.pixels {
            Pixels::Image(_) => 0,
            Pixels::Bytes(_, _, b) => b.len(),
        }
    }
}

struct UnpackJob {
    key: FrameKey,
    packed: Arc<PackedFrame>,
}

#[derive(Default)]
struct State {
    pack: VecDeque<PackJob>,
    /// [`PackJob::held`] of the queued pack jobs.
    held: usize,
    unpack: VecDeque<UnpackJob>,
    running: usize,
}

#[derive(Default)]
pub(super) struct Codec {
    state: Mutex<State>,
    /// The cache results go to (set once by `Frames::default`).
    cache: Mutex<Weak<Mutex<Cache>>>,
}

impl Codec {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Send results to `cache`.
    pub(super) fn bind(&self, cache: &Arc<Mutex<Cache>>) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(cache);
    }

    /// Queue a frame to compress; `false` when the queue is full or there are no threads (the
    /// frame stays uncompressed).
    pub(super) fn pack(self: &Arc<Self>, job: PackJob) -> bool {
        if cfg!(target_arch = "wasm32") {
            return false;
        }
        let mut s = self.lock();
        let held = job.held();
        if s.pack.len() >= MAX_PACK_JOBS || (held > 0 && s.held.saturating_add(held) > MAX_PACK_BYTES) || s.pack.try_reserve(1).is_err() {
            return false;
        }
        s.held = s.held.saturating_add(held);
        s.pack.push_back(job);
        self.start(&mut s);
        true
    }

    /// Queue a compressed frame to decode ahead of the playhead; `false` when it can't be.
    pub(super) fn unpack(self: &Arc<Self>, key: FrameKey, packed: Arc<PackedFrame>) -> bool {
        if cfg!(target_arch = "wasm32") {
            return false;
        }
        let mut s = self.lock();
        if s.unpack.len() >= MAX_UNPACK_JOBS || s.unpack.try_reserve(1).is_err() {
            return false;
        }
        s.unpack.push_back(UnpackJob { key, packed });
        self.start(&mut s);
        true
    }

    /// Drop every queued job (Purge Caches); jobs running finish into a cache that no longer
    /// has their frames.
    pub(super) fn clear(&self) {
        let mut s = self.lock();
        s.pack.clear();
        s.unpack.clear();
        s.held = 0;
    }

    /// Jobs queued (pack, unpack): diagnostics and tests.
    pub(super) fn queued(&self) -> (usize, usize) {
        let s = self.lock();
        (s.pack.len(), s.unpack.len())
    }

    /// Start a thread when one more may run.
    fn start(self: &Arc<Self>, s: &mut State) {
        if s.running >= THREADS || s.running >= s.pack.len().saturating_add(s.unpack.len()) {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("ec-frame-codec".into()).spawn(move || {
            // Background priority (the app's hook: macOS utility QoS), so compression never takes
            // CPU from the render workers, the UI or playback.
            super::idle::thread_start();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| me.run())).is_err() {
                log::error!("a RAM preview compression thread panicked; its frames stay uncompressed");
                let mut s = me.lock();
                s.running = s.running.saturating_sub(1);
            }
        });
        if spawned.is_ok() {
            s.running += 1;
        }
    }

    /// A codec thread: decode ahead of the playhead first, then compress; exits when both
    /// queues are empty.
    fn run(&self) {
        loop {
            enum Next {
                Unpack(UnpackJob),
                Pack(PackJob),
            }
            let next = {
                let mut s = self.lock();
                if let Some(j) = s.unpack.pop_front() {
                    Next::Unpack(j)
                } else if let Some(j) = s.pack.pop_front() {
                    s.held = s.held.saturating_sub(j.held());
                    Next::Pack(j)
                } else {
                    s.running = s.running.saturating_sub(1);
                    return;
                }
            };
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner).upgrade();
            match next {
                Next::Unpack(UnpackJob { key, packed }) => {
                    let img = unpack_image(&packed, 1);
                    if let Some(cache) = cache {
                        let mut c = cache.lock().unwrap_or_else(PoisonError::into_inner);
                        match img {
                            Some(img) => c.attach_raw(&key, &packed, FrameImage::Cpu(Arc::new(img))),
                            None => {
                                c.unpacking.remove(&key);
                            }
                        }
                    }
                }
                Next::Pack(PackJob { key, raw_id, pixels }) => {
                    let packed = match &pixels {
                        Pixels::Image(img) => PackedFrame::encode(img.size[0] as u32, img.size[1] as u32, img.as_raw()),
                        Pixels::Bytes(w, h, b) => PackedFrame::encode(*w, *h, b),
                    };
                    // The read-back bytes go before the cache is locked.
                    drop(pixels);
                    if let Some(cache) = cache {
                        let mut c = cache.lock().unwrap_or_else(PoisonError::into_inner);
                        match packed {
                            Some(p) => c.attach_packed(key, raw_id, Arc::new(p)),
                            None => {
                                c.packing.remove(&key);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A compressed frame's pixels as an egui image, decoded on up to `threads` threads; `None`
/// when it is damaged or memory runs out.
pub(super) fn unpack_image(p: &PackedFrame, threads: usize) -> Option<egui::ColorImage> {
    let (w, h) = (usize::try_from(p.width()).ok()?, usize::try_from(p.height()).ok()?);
    let n = w.checked_mul(h)?;
    if n.checked_mul(4)? != p.raw_len() {
        return None;
    }
    let mut px = Vec::new();
    px.try_reserve_exact(n).ok()?;
    px.resize(n, egui::Color32::TRANSPARENT);
    let mut img = egui::ColorImage::new([w, h], px);
    p.decode_into(img.as_raw_mut(), threads).then_some(img)
}
