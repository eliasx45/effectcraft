//! Compressed RAM preview frames, spill on eviction and disk read-ahead
//! (`plan/memory/compressed-cache.md`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

fn key(frame: i64) -> FrameKey {
    FrameKey { revision: 1, content: 1, comp: 1, frame, scale: 1000, view: 0, opts: 0 }
}

/// Premultiplied pixels with flat areas (they compress), noise (it doesn't) and transparent
/// holes, different for each `seed`.
fn image(w: usize, h: usize, seed: u32) -> Arc<egui::ColorImage> {
    let px = (0..w * h)
        .map(|i| {
            let (x, y) = ((i % w) as u32, (i / w) as u32);
            let noise = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed.wrapping_mul(97)) >> 24;
            let a: u8 = if (x / 7 + y / 5 + seed) % 4 == 0 {
                0
            } else if y % 9 == 0 {
                128
            } else {
                255
            };
            let flat = ((x / 16 + seed) * 40 % 256) as u8;
            // Noise on every third pixel only: real frames compress, and so does this.
            let n = if i % 3 == 0 { (noise % 256) as u8 } else { flat };
            egui::Color32::from_rgba_premultiplied(flat.min(a), n.min(a), (x as u8 ^ y as u8).min(a), a)
        })
        .collect();
    Arc::new(egui::ColorImage::new([w, h], px))
}

/// A frame of one colour per `seed`: it compresses to almost nothing.
fn flat(w: usize, h: usize, seed: u32) -> Arc<egui::ColorImage> {
    let c = egui::Color32::from_rgba_premultiplied((seed * 20 % 256) as u8, 40, 200, 255);
    Arc::new(egui::ColorImage::new([w, h], vec![c; w * h]))
}

/// Wait until the codec has nothing queued or on its way.
fn settle(f: &Frames) {
    let t0 = std::time::Instant::now();
    loop {
        let idle = {
            let c = f.cache.lock().unwrap();
            c.codec.queued() == (0, 0) && c.packing.is_empty() && c.unpacking.is_empty()
        };
        if idle {
            return;
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the codec never finished");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Every count the cache keeps matches its frames.
fn assert_counts(f: &Frames) {
    let c = f.cache.lock().unwrap();
    let bytes: usize = c.map.values().map(Cached::bytes).sum();
    let packed: usize = c.map.values().filter_map(|s| s.packed.as_ref()).map(|p| p.bytes()).sum();
    let packed_raw: usize = c.map.values().filter_map(|s| s.packed.as_ref()).map(|p| p.raw_len()).sum();
    let packed_frames = c.map.values().filter(|s| s.packed.is_some()).count();
    assert_eq!((c.bytes, c.packed_bytes, c.packed_raw, c.packed_frames), (bytes, packed, packed_raw, packed_frames));
    assert_eq!(c.meter.load(Ordering::Relaxed), bytes as u64);
    assert!(c.map.values().all(|s| s.raw.is_some() || s.packed.is_some()), "every frame has pixels or bytes");
}

fn cpu_pixels(img: &FrameImage) -> Vec<egui::Color32> {
    match img {
        FrameImage::Cpu(c) => c.pixels.clone(),
        FrameImage::Gpu(_) => panic!("CPU frames"),
    }
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("effectcraft-compressed-cache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// The pixels of one 64×36 frame.
const SMALL: usize = 64 * 36 * 4;

/// Frames (sorted) holding pixels / compressed bytes.
fn raw_frames(f: &Frames) -> Vec<i64> {
    let c = f.cache.lock().unwrap();
    let mut v: Vec<i64> = c.map.iter().filter(|(_, s)| s.raw.is_some()).map(|(k, _)| k.frame).collect();
    v.sort_unstable();
    v
}
fn packed_frames(f: &Frames) -> Vec<i64> {
    let c = f.cache.lock().unwrap();
    let mut v: Vec<i64> = c.map.iter().filter(|(_, s)| s.packed.is_some()).map(|(k, _)| k.frame).collect();
    v.sort_unstable();
    v
}

/// Set the budget without evicting (the frames stay; only the uncompressed share is trimmed).
fn squeeze(f: &Frames, budget: usize) {
    let mut c = f.cache.lock().unwrap();
    c.budget = budget;
    c.trim_raw();
}

/// A frame compressed in RAM decodes to exactly the pixels it was rendered with, at odd sizes
/// too (fewer rows than strips, widths not a multiple of anything).
#[test]
fn compressed_frames_decode_to_the_exact_pixels() {
    let f = Frames::default();
    f.set_compress(true, 0);
    let sizes = [(1, 1), (3, 7), (33, 15), (64, 64), (190, 107)];
    let originals: Vec<Arc<egui::ColorImage>> = sizes.iter().enumerate().map(|(i, (w, h))| image(*w, *h, i as u32)).collect();
    for (i, img) in originals.iter().enumerate() {
        f.cache.lock().unwrap().insert(key(i as i64), FrameImage::Cpu(img.clone()));
    }
    settle(&f);
    assert!(packed_frames(&f).is_empty(), "room to spare: nothing compressed");
    // The pixels fill the budget exactly: past the headroom, so the oldest are compressed.
    let total: usize = originals.iter().map(|i| i.pixels.len() * 4).sum();
    f.set_budget(total);
    settle(&f);
    {
        let c = f.cache.lock().unwrap();
        assert!(c.packed_frames >= 1);
        let oldest = c.map.get(&key(0)).unwrap();
        assert!(oldest.packed.is_some() && oldest.raw.is_none(), "the oldest frame keeps only its bytes");
    }
    assert_counts(&f);
    for (i, img) in originals.iter().enumerate() {
        let shown = f.get(&key(i as i64)).expect("cached");
        assert_eq!(cpu_pixels(&shown), img.pixels, "frame {i} decodes exactly");
    }
    assert_counts(&f);
}

/// Nothing is compressed while the pixels stay within [`PACK_AT_PERCENT`] of the budget; one
/// byte of budget less and the least recently shown frame is, ahead of eviction (nothing is
/// evicted), and it drops its pixels when it lands.
#[test]
fn packing_starts_at_the_headroom_threshold() {
    let f = Frames::default();
    f.set_compress(true, 0);
    for i in 0..10 {
        f.cache.lock().unwrap().insert(key(i), FrameImage::Cpu(image(64, 36, i as u32)));
    }
    let raw = 10 * SMALL;
    // The smallest budget whose share still holds the pixels.
    let at = (raw * 100).div_ceil(PACK_AT_PERCENT);
    assert!(share_of(at, PACK_AT_PERCENT) >= raw && share_of(at - 1, PACK_AT_PERCENT) < raw);
    f.set_budget(at);
    settle(&f);
    assert!(packed_frames(&f).is_empty(), "at the threshold: nothing compressed");
    assert_eq!(f.cache.lock().unwrap().codec.queued(), (0, 0));
    f.set_budget(at - 1);
    settle(&f);
    assert_eq!(packed_frames(&f), vec![0], "past it: the least recently shown frame only");
    assert_eq!(raw_frames(&f), (1..10).collect::<Vec<_>>());
    assert_eq!(f.memory_usage(false).frames, 10, "compressed ahead of eviction: nothing evicted");
    assert_counts(&f);
}

/// A playing loop whose frames all fit the budget as pixels is never compressed, however close
/// to the budget it is; a range that doesn't fit is.
#[test]
fn a_loop_that_fits_is_never_compressed() {
    let f = Frames::default();
    f.set_compress(true, 0);
    for i in 0..10 {
        f.cache.lock().unwrap().insert(key(i), FrameImage::Cpu(image(64, 36, i as u32)));
    }
    let play = |end: i64| Playhead { series: key(0), cur: 0, start: 0, end, step: 1, looping: true };
    f.set_playhead(Some(play(9)));
    // Exactly the loop's pixels: far past the headroom, and nothing to evict.
    f.set_budget(10 * SMALL);
    settle(&f);
    assert!(packed_frames(&f).is_empty(), "the loop fits: no frame of it is compressed");
    assert_eq!(raw_frames(&f), (0..10).collect::<Vec<_>>());
    assert_eq!(f.memory_usage(false).frames, 10);
    // The same frames in a range twice as long (it can't fit): compressed, the frames the
    // preview reaches last first; the one on screen keeps its pixels.
    f.set_playhead(Some(play(19)));
    settle(&f);
    let packed = packed_frames(&f);
    assert!(!packed.is_empty() && !packed.contains(&0), "{packed:?}");
    assert!(packed.iter().all(|p| (*p..10).all(|q| packed.contains(&q))), "farthest first: {packed:?}");
    assert_counts(&f);
}

/// The budget counts what each frame holds (pixels, compressed bytes or both), through inserts,
/// compression, the trim of pixels, eviction, a purge and clearing.
#[test]
fn the_budget_counts_compressed_bytes_through_inserts_and_evictions() {
    let f = Frames::default();
    f.set_compress(true, 0);
    f.set_budget(10 * 128 * 72 * 4);
    for i in 0..10 {
        f.cache.lock().unwrap().insert(key(i), FrameImage::Cpu(image(128, 72, i as u32)));
        assert_counts(&f);
        settle(&f);
        assert_counts(&f);
    }
    let u = f.memory_usage(false);
    assert_eq!(u.frames, 10, "compressed ahead of time: nothing evicted");
    assert!(u.compress && u.packed_frames >= 1);
    assert_eq!(u.raw_bytes + u.packed_bytes, u.bytes);
    // A frame compressed because it was leaving the uncompressed share keeps only its bytes.
    assert!(f.cache.lock().unwrap().map.values().all(|s| s.packed.is_none() || s.raw.is_none()));
    assert!(u.packed_bytes < u.packed_raw_bytes, "compressed bytes are fewer than the pixels");
    // A smaller budget evicts whole frames; the counts follow.
    let keep = u.bytes / 2;
    f.set_budget(keep as usize);
    assert_counts(&f);
    settle(&f);
    assert_counts(&f);
    let after = f.memory_usage(false);
    assert!(after.bytes <= keep && after.frames < 10);
    let (_, purge) = f.memory_hooks();
    assert_eq!(purge(Some(1)), after.bytes);
    assert_counts(&f);
    f.clear();
    let c = f.cache.lock().unwrap();
    assert_eq!((c.bytes, c.packed_bytes, c.packed_raw, c.packed_frames, c.rawset.len(), c.gpu_wanted.len()), (0, 0, 0, 0, 0, 0));
}

/// The frames the playing preview reaches next keep (and get back) their pixels; the others
/// are compressed when the budget runs short, the ones it doesn't reach first.
#[test]
fn the_window_ahead_of_the_playhead_keeps_its_pixels() {
    let f = Frames::default();
    f.set_compress(true, 2);
    // Flat frames: their compressed bytes take next to no room, so nothing is evicted.
    let originals: Vec<_> = (0..10).map(|i| flat(64, 36, i)).collect();
    for (i, img) in originals.iter().enumerate() {
        f.cache.lock().unwrap().insert(key(i as i64), FrameImage::Cpu(img.clone()));
    }
    // A range longer than the cache holds (the loop doesn't fit), not looping.
    let at = |cur: i64| Playhead { series: key(0), cur, start: 0, end: 19, step: 1, looping: false };
    f.set_playhead(Some(at(3)));
    settle(&f);
    assert!(packed_frames(&f).is_empty(), "room to spare");
    // Room for six frames' pixels: every frame but the window (3, 4, 5) is compressed.
    squeeze(&f, 6 * SMALL);
    settle(&f);
    assert_eq!(raw_frames(&f), vec![3, 4, 5]);
    assert_eq!(packed_frames(&f), vec![0, 1, 2, 6, 7, 8, 9]);
    // Moving back: 0, 1, 2 are decoded ahead of it, exactly.
    f.set_playhead(Some(at(0)));
    settle(&f);
    assert!([0, 1, 2].iter().all(|i| raw_frames(&f).contains(i)), "{:?}", raw_frames(&f));
    for i in 0..3 {
        let c = f.cache.lock().unwrap();
        assert_eq!(cpu_pixels(c.map.get(&key(i)).unwrap().raw.as_ref().unwrap()), originals[i as usize].pixels);
    }
    assert_counts(&f);
}

/// A frame worth keeping is written to disk when it is evicted (its compressed bytes as they
/// are, or its pixels when compression is off), and a disk hit gives back the same pixels. A
/// cheap frame (no disk target) is not written.
#[test]
fn evicted_frames_spill_to_disk_and_read_back_identical() {
    for compress in [true, false] {
        let dir = tmpdir(if compress { "spill-packed" } else { "spill-raw" });
        let dc = DiskCache::open(&dir, 1 << 30).unwrap();
        let f = Frames::default();
        f.set_compress(compress, 0);
        let worth = image(96, 54, 1);
        let cheap = image(96, 54, 2);
        let disk = DiskPlan { target: Some((dc.clone(), 0xfeed)), now: false };
        {
            let mut c = f.cache.lock().unwrap();
            c.insert_stored(key(0), FrameImage::Cpu(worth.clone()), Stored { disk, packed: None });
            c.insert(key(1), FrameImage::Cpu(cheap));
        }
        settle(&f);
        if compress {
            // Past the headroom: the least recently shown frame (the one worth keeping) is
            // compressed ahead of its eviction, and spills as those bytes.
            f.set_budget(f.memory_usage(false).bytes as usize);
            settle(&f);
            assert!(f.cache.lock().unwrap().map.get(&key(0)).unwrap().packed.is_some());
        }
        dc.flush();
        assert!(!dc.contains(disk_cache::Kind::Frame, 0xfeed), "not written before it leaves RAM");
        let ram = f.cache.lock().unwrap().map.get(&key(0)).unwrap().packed.clone();
        f.set_budget(0);
        dc.flush();
        assert!(f.cache.lock().unwrap().map.is_empty());
        assert_eq!(f.memory_usage(false).spilled, 1, "only the frame worth keeping");
        assert_eq!(dc.stats().entries, 1);
        let back = dc.get_frame(0xfeed).expect("on disk");
        assert_eq!((back.width, back.height), (96, 54));
        assert_eq!(back.rgba, worth.as_raw(), "identical pixels (compress {compress})");
        // The RAM preview reads the same bytes back without compressing them again.
        let packed = dc.get_packed_frame(0xfeed).unwrap();
        assert_eq!(packed.decode(3).unwrap(), worth.as_raw());
        if let Some(ram) = ram {
            assert_eq!(packed.payload(), ram.payload(), "spilled as the bytes RAM kept");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A frame to write now goes to disk once it is compressed, from the compressed bytes, and has
/// nothing left to spill.
#[test]
fn frames_written_now_go_to_disk_from_the_compressed_bytes() {
    let dir = tmpdir("write-now");
    let dc = DiskCache::open(&dir, 1 << 30).unwrap();
    let f = Frames::default();
    f.set_compress(true, 0);
    let img = image(80, 45, 9);
    let disk = DiskPlan { target: Some((dc.clone(), 0xbeef)), now: true };
    f.cache.lock().unwrap().insert_stored(key(0), FrameImage::Cpu(img.clone()), Stored { disk, packed: None });
    settle(&f);
    dc.flush();
    let ram = f.cache.lock().unwrap().map.get(&key(0)).unwrap().packed.clone().unwrap();
    assert!(f.cache.lock().unwrap().map.get(&key(0)).unwrap().disk.target.is_none(), "nothing left to spill");
    assert_eq!(dc.get_packed_frame(0xbeef).unwrap().payload(), ram.payload(), "the bytes RAM keeps");
    assert_eq!(dc.get_frame(0xbeef).unwrap().rgba, img.as_raw());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A queued prefetch frame the disk cache holds is read ahead (not rendered): it leaves the
/// queue, lands in RAM compressed, and shows the pixels the disk holds.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn queued_frames_on_disk_are_read_ahead_instead_of_rendered() {
    use effectcraft_engine::Session;
    use serde_json::json;
    let mut s = Session::default();
    let cid = ItemId(s.execute("comp.new", json!({"name": "A", "width": 48, "height": 32, "duration": 2})).unwrap()["comp"].as_u64().unwrap());
    s.execute("layer.newSolid", json!({"color": "#3080ff", "width": 20, "height": 12})).unwrap();
    let dir = tmpdir("read-ahead");
    let dc = DiskCache::open(&dir, 1 << 30).unwrap();
    let src = RenderSource {
        freeze_feedback: Default::default(),
        project: s.project.clone(),
        footage: Arc::new(effectcraft_engine::render::NoFootage),
        expr: None,
        layer_cache: Arc::new(LayerCache::new(64 << 20)),
        gpu: None,
        gpu_display: false,
        disk: Some(dc.clone()),
    };
    let f = Frames::default();
    f.set_compress(true, 0);
    let content = f.content_of(&s.project, s.revision, cid);
    let k = |frame: i64| FrameKey { revision: s.revision, content, comp: cid.0, frame, scale: 1000, view: 0, opts: opts_hash(&RenderOpts::default()) };
    let opts = RenderOpts::default();
    let t3 = s.project.comp(cid).unwrap().frame_rate.tick_of(3);
    // Frame 3 on disk only.
    assert!(f.request_to_disk(&src, k(3), cid, t3, opts));
    for _ in 0..500 {
        if f.inflight() == 0 && f.disk_frames(&src, &k(0), 10, &opts) == vec![3] {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    dc.flush();
    assert!(!f.is_cached(&k(3)));
    // Queued for playback (not started), then looked up by the read-ahead.
    let job = Job {
        key: k(3),
        src: src.clone(),
        comp: cid,
        t: t3,
        opts,
        urgent: false,
        disk_only: false,
        idle: false,
        seq: 99,
        epoch: 0,
        on_gpu: None,
        retry: None,
        cancel: Arc::default(),
        print: None,
    };
    {
        let mut q = f.queue.lock().unwrap();
        f.inflight.lock().unwrap().insert(k(3));
        q.jobs.push(job.clone());
    }
    f.worker().read_ahead_one(job);
    assert!(f.queue.lock().unwrap().jobs.iter().all(|j| j.seq != 99), "taken out of the render queue");
    assert!(!f.inflight.lock().unwrap().contains(&k(3)));
    assert!(f.is_cached(&k(3)));
    let u = f.memory_usage(false);
    assert_eq!((u.read_ahead, u.packed_frames), (1, 1));
    // The pixels are the disk's.
    let print = fingerprint::of(&src, cid, t3, opts).unwrap();
    let disk_key = frame_keys::print_disk_key(&f.salts, &src.project, &k(3), print, opts_hash(&opts));
    let on_disk = dc.get_frame(disk_key).unwrap();
    let shown = f.get(&k(3)).unwrap();
    let FrameImage::Cpu(shown) = shown else { panic!("CPU frame") };
    assert_eq!(shown.as_raw(), &on_disk.rgba[..]);
    let _ = std::fs::remove_dir_all(&dir);
}
