//! The frame server's own preview proxies of big clips (PERF-PROXY): a movie of the replica's
//! project over [`MIN_MB`] (Elias's 6.3 GB ProRes 4444 presenter clip) is hashed and rebuilt in
//! the background at half size ([`effectcraft_media::proxy_build`]: ProRes 4444 with alpha, the
//! file's own Y'CbCr codes box-averaged, nothing uploaded from the Mac). Once it is ready, frames
//! at Half resolution or below read it as the item's proxy (the same path as File ▸ Set Proxy),
//! which decodes a quarter of the samples; Full-resolution frames and every export keep the
//! original. Preview only: a proxy frame is visually the same, not bit-identical (ProRes is
//! lossy and the halving is a box mean, where the original is resampled by the renderer).
//!
//! Proxies live in `EFFECTCRAFT_SERVER_PROXY_DIR` (else the disk cache folder's
//! `server-proxies`), named by the clip's sha256, so a renamed or re-synced copy of the same clip
//! reuses one. A build starts only when the volume keeps the disk cache's reserve free after it
//! (`disk_cache::reserve_bytes`). `EFFECTCRAFT_AUTO_PROXY=0` turns it off; `…_MIN_MB` sets the
//! size from which clips get one; `EFFECTCRAFT_PROXY_THREADS` the build's threads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Instant;

use effectcraft_automation::frame_server::{FrameRender, LogFn, RenderJob, Rendered};
use effectcraft_engine::cloud_assets;
use effectcraft_engine::project::render_queue::ProxyUse;
use effectcraft_engine::project::{Footage, FootageKind, ItemId, ItemKind, Project, Proxy};
use effectcraft_engine::time::Tick;
use serde_json::{Value, json};

/// Clips from this size (MB) get a proxy.
const MIN_MB: u64 = 1024;
/// The proxy's reduction: Half.
const DIVISOR: u32 = 2;

/// Where a clip's proxy stands.
#[derive(Clone, Debug, PartialEq)]
enum State {
    /// Hashing or building.
    Working,
    Ready(Ready),
    /// Not made (no ProRes, no disk space, a decode error); not tried again this run.
    Skipped(String),
}

#[derive(Clone, Debug, PartialEq)]
struct Ready {
    path: String,
    width: u32,
    height: u32,
}

struct Shared {
    dir: PathBuf,
    min_bytes: u64,
    threads: usize,
    /// Per source path.
    clips: Mutex<HashMap<String, State>>,
    /// Bumped when a proxy becomes ready (the patched project is rebuilt).
    generation: AtomicU64,
    stop: AtomicBool,
    log: LogFn,
}

/// The project as frames read it: the replica's with ready proxies set on its big clips.
struct Patched {
    key: (usize, usize, u64, u64),
    project: Option<Arc<Project>>,
}

pub struct AutoProxy {
    inner: Arc<dyn FrameRender>,
    shared: Arc<Shared>,
    queue: Mutex<mpsc::Sender<(String, u64)>>,
    last: Mutex<Option<Patched>>,
}

fn env_u64(k: &str) -> Option<u64> {
    std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok())
}

/// `render` with automatic proxies, unless `EFFECTCRAFT_AUTO_PROXY=0`.
pub fn wrap(render: Arc<dyn FrameRender>, log: LogFn) -> Arc<dyn FrameRender> {
    if std::env::var("EFFECTCRAFT_AUTO_PROXY").is_ok_and(|v| v.trim() == "0") {
        return render;
    }
    let dir = std::env::var_os("EFFECTCRAFT_SERVER_PROXY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| effectcraft_render::disk_cache::default_folder().join("server-proxies"));
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = env_u64("EFFECTCRAFT_PROXY_THREADS").and_then(|n| usize::try_from(n).ok()).filter(|n| *n >= 1).unwrap_or((cores / 4).max(2));
    let min_bytes = env_u64("EFFECTCRAFT_AUTO_PROXY_MIN_MB").unwrap_or(MIN_MB).saturating_mul(1 << 20);
    Arc::new(new(render, dir, min_bytes, threads, log))
}

/// [`wrap`] for benchmarks: the proxies of `project`'s big clips made (or found) before it
/// returns, so the frames measured read them. Gives up after `wait`.
pub fn wrap_ready(render: Arc<dyn FrameRender>, project: &Project, wait: std::time::Duration, log: LogFn) -> Arc<dyn FrameRender> {
    let dir = std::env::var_os("EFFECTCRAFT_SERVER_PROXY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| effectcraft_render::disk_cache::default_folder().join("server-proxies"));
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let min_bytes = env_u64("EFFECTCRAFT_AUTO_PROXY_MIN_MB").unwrap_or(MIN_MB).saturating_mul(1 << 20);
    let auto = new(render, dir, min_bytes, cores, log.clone());
    let t0 = Instant::now();
    let _ = auto.patched(project);
    loop {
        let working = auto.shared.clips.lock().unwrap_or_else(PoisonError::into_inner).values().filter(|s| **s == State::Working).count();
        if working == 0 || t0.elapsed() > wait {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let ready = auto.shared.clips.lock().unwrap_or_else(PoisonError::into_inner).values().filter(|s| matches!(s, State::Ready(_))).count();
    log(&format!("proxies: {ready} ready after {:.0} s", t0.elapsed().as_secs_f64()));
    Arc::new(auto)
}

fn new(inner: Arc<dyn FrameRender>, dir: PathBuf, min_bytes: u64, threads: usize, log: LogFn) -> AutoProxy {
    let shared = Arc::new(Shared { dir, min_bytes, threads, clips: Mutex::default(), generation: AtomicU64::new(0), stop: AtomicBool::new(false), log });
    let (tx, rx) = mpsc::channel::<(String, u64)>();
    let worker = shared.clone();
    let spawned = std::thread::Builder::new().name("auto-proxy".into()).spawn(move || {
        while let Ok((path, size)) = rx.recv() {
            if worker.stop.load(Ordering::Relaxed) {
                break;
            }
            let state = make(&worker, &path, size);
            if let State::Skipped(why) = &state {
                (worker.log)(&format!("proxy: {path}: {why}"));
            }
            let ready = matches!(state, State::Ready(_));
            worker.clips.lock().unwrap_or_else(PoisonError::into_inner).insert(path, state);
            if ready {
                worker.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    if let Err(e) = spawned {
        (shared.log)(&format!("proxy: no worker thread ({e}); frames read the originals"));
    }
    AutoProxy { inner, shared, queue: Mutex::new(tx), last: Mutex::new(None) }
}

impl Drop for AutoProxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Whether a frame may read proxies: Half resolution or below, under a Proxy Use that reads
/// footage proxies (the viewer's is Current Settings).
fn may_use(scale: f64, proxy: ProxyUse) -> bool {
    scale <= 1.0 / f64::from(DIVISOR) + 1e-6 && matches!(proxy, ProxyUse::CurrentSettings | ProxyUse::UseAll)
}

/// A movie big enough for a proxy (and the decoder can rebuild), with no proxy of its own.
fn candidate(f: &Footage, has_proxy: bool, min_bytes: u64) -> Option<u64> {
    if has_proxy || f.missing || f.kind != FootageKind::Video || !f.has_video || !f.sequence.is_empty() || !effectcraft_media::proxy_build::supports(&f.codec) {
        return None;
    }
    if f.width < 2 * 256 || f.height < 2 * 144 {
        return None;
    }
    let size = std::fs::metadata(&f.path).ok()?.len();
    (size >= min_bytes).then_some(size)
}

/// The proxy footage: the original's interpretation (rate, alpha, colour profile…) on the proxy
/// file. The renderer scales it up to the item's size.
fn proxy_footage(original: &Footage, r: &Ready) -> Footage {
    Footage { path: r.path.clone(), width: r.width, height: r.height, has_audio: false, missing: false, ..original.clone() }
}

/// Free and total bytes of the volume holding `dir`.
fn volume(dir: &Path) -> Option<(u64, u64)> {
    let st = fs4::statvfs(dir).ok()?;
    (st.total_space() > 0).then(|| (st.available_space(), st.total_space()))
}

/// Hash `path`, then reuse or build its proxy.
fn make(sh: &Shared, path: &str, size: u64) -> State {
    let t0 = Instant::now();
    // (Remembered by size + mtime: the mirror has usually hashed the clip already.)
    let Some(sha) = cloud_assets::hash_file(path, &mut |_| {}) else {
        return State::Skipped("can't read it to hash it".into());
    };
    let hash_s = t0.elapsed().as_secs_f64();
    let stem = format!("{sha}-half{DIVISOR}");
    let out = sh.dir.join(format!("{stem}.mov"));
    let meta = sh.dir.join(format!("{stem}.json"));
    if let Some(r) = read_meta(&meta, &out) {
        (sh.log)(&format!("proxy: {path}: reusing {} ({}×{})", out.display(), r.width, r.height));
        return State::Ready(r);
    }
    if let Err(e) = std::fs::create_dir_all(&sh.dir) {
        return State::Skipped(format!("can't make {} ({e})", sh.dir.display()));
    }
    // Half size at the source's density: about a quarter of the bytes; a margin.
    let estimate = size / u64::from(DIVISOR * DIVISOR) + size / 16;
    if let Some((free, total)) = volume(&sh.dir) {
        let reserve = effectcraft_render::disk_cache::reserve_bytes(total);
        if free.saturating_sub(estimate) < reserve {
            return State::Skipped(format!(
                "not enough disk for its proxy (~{} needed, {} free, {} kept free)",
                cloud_assets::human_bytes(estimate),
                cloud_assets::human_bytes(free),
                cloud_assets::human_bytes(reserve)
            ));
        }
    }
    (sh.log)(&format!("proxy: {path}: building a half-size proxy ({} hashed in {hash_s:.1} s, {} threads)", cloud_assets::human_bytes(size), sh.threads));
    let pool = match rayon::ThreadPoolBuilder::new().num_threads(sh.threads).thread_name(|i| format!("proxy-{i}")).build() {
        Ok(p) => p,
        Err(e) => return State::Skipped(format!("no build threads ({e})")),
    };
    let t1 = Instant::now();
    let mut tenth = 0u64;
    let log = sh.log.clone();
    let built = pool.install(|| {
        effectcraft_media::proxy_build::build(path, &out, DIVISOR, &sh.stop, &mut |done, total| {
            let t = done.saturating_mul(10) / total.max(1);
            if t > tenth {
                tenth = t;
                let fps = done as f64 / t1.elapsed().as_secs_f64().max(1e-3);
                log(&format!("proxy: {path}: {}% ({done}/{total} frames, {fps:.0} fps)", t * 10));
            }
        })
    });
    match built {
        Ok(b) => {
            let r = Ready { path: out.to_string_lossy().into_owned(), width: b.width, height: b.height };
            let doc = json!({"source": path, "sha256": sha, "width": b.width, "height": b.height, "frames": b.frames, "bytes": b.bytes, "divisor": DIVISOR});
            if let Err(e) = std::fs::write(&meta, doc.to_string()) {
                (sh.log)(&format!("proxy: can't write {} ({e}); it is used this run only", meta.display()));
            }
            (sh.log)(&format!(
                "proxy: {path}: ready, {}×{} ProRes, {} frames, {} in {:.0} s",
                b.width,
                b.height,
                b.frames,
                cloud_assets::human_bytes(b.bytes),
                t1.elapsed().as_secs_f64()
            ));
            State::Ready(r)
        }
        Err(e) => State::Skipped(format!("no proxy ({e})")),
    }
}

/// A proxy built on an earlier run (its description and movie both there).
fn read_meta(meta: &Path, movie: &Path) -> Option<Ready> {
    let v: Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    if !movie.is_file() {
        return None;
    }
    let dim = |k: &str| v.get(k).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0);
    Some(Ready { path: movie.to_string_lossy().into_owned(), width: dim("width")?, height: dim("height")? })
}

impl AutoProxy {
    /// The project frames read: `None` while no proxy of it is ready. New big clips are queued.
    fn patched(&self, project: &Project) -> Option<Arc<Project>> {
        let generation = self.shared.generation.load(Ordering::Relaxed);
        // The replica's project: a new one per revision (identity + a cheap fingerprint).
        let key = (std::ptr::from_ref(project) as usize, project.items.len(), project.next_id, generation);
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(p) = last.as_ref().filter(|p| p.key == key) {
            return p.project.clone();
        }
        let mut ready: Vec<(ItemId, Footage)> = vec![];
        {
            let mut clips = self.shared.clips.lock().unwrap_or_else(PoisonError::into_inner);
            for (id, it) in &project.items {
                let ItemKind::Footage(f) = &it.kind else { continue };
                match clips.get(&f.path) {
                    Some(State::Ready(r)) if it.proxy.is_none() => ready.push((*id, proxy_footage(f, r))),
                    Some(_) => {}
                    None => {
                        if let Some(size) = candidate(f, it.proxy.is_some(), self.shared.min_bytes) {
                            clips.insert(f.path.clone(), State::Working);
                            let sent = self.queue.lock().unwrap_or_else(PoisonError::into_inner).send((f.path.clone(), size));
                            if sent.is_err() {
                                clips.insert(f.path.clone(), State::Skipped("no worker".into()));
                            }
                        }
                    }
                }
            }
        }
        let project = (!ready.is_empty()).then(|| {
            let mut p = project.clone();
            for (id, footage) in ready {
                if let Some(it) = p.item_mut(id) {
                    it.proxy = Some(Box::new(Proxy { footage, enabled: true, auto: false }));
                }
            }
            Arc::new(p)
        });
        *last = Some(Patched { key, project: project.clone() });
        project
    }
}

impl FrameRender for AutoProxy {
    fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        // Every frame queues new big clips; only Half and below read the proxies.
        let patched = self.patched(job.project);
        match patched.filter(|_| may_use(job.opts.scale, job.opts.proxy)) {
            Some(p) => self.inner.render(&RenderJob { project: &p, ..*job }),
            None => self.inner.render(job),
        }
    }

    fn gpu_name(&self) -> Option<String> {
        self.inner.gpu_name()
    }

    fn warm(&self, project: &Project, comp: ItemId, time: Tick) {
        self.inner.warm(project, comp, time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_engine::render::{LayerCache, RenderOpts};

    /// Records the proxy each frame read for the first footage item.
    struct Spy(Mutex<Vec<Option<String>>>);
    impl FrameRender for Spy {
        fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
            let px = job
                .project
                .items
                .values()
                .find_map(|it| matches!(it.kind, ItemKind::Footage(_)).then(|| it.proxy.as_ref().map(|p| p.footage.path.clone())))
                .flatten();
            self.0.lock().unwrap().push(px);
            Ok((Rendered::Rgba8 { width: 1, height: 1, bytes: vec![0; 4] }, false))
        }
    }

    fn movie(path: &Path, w: u32, h: u32, n: u32) {
        use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
        use filmcraft_prores::{ChromaFormat, Encoder, Frame, Profile};
        let mut enc = Encoder::new(Profile::P4444, w, h);
        let mut mux = Mp4Writer::new(std::io::BufWriter::new(std::fs::File::create(path).unwrap()), WriterOptions::new(Brand::Mov)).unwrap();
        let vt = mux.add_track(TrackConfig::new(SampleEntry::prores(FourCc(*b"ap4h"), w as u16, h as u16), 30000)).unwrap();
        for _ in 0..n {
            let data = enc.encode(&Frame::new(w, h, ChromaFormat::Yuv444, 12, true)).unwrap();
            mux.write_sample(vt, WriteSample { data: &data, duration: 1001, composition_offset: 0, is_sync: true }).unwrap();
        }
        use std::io::Write;
        mux.finish().unwrap().flush().unwrap();
    }

    #[test]
    fn half_resolution_frames_read_the_proxy_once_built_and_full_ones_never() {
        let dir = std::env::temp_dir().join(format!("ec-auto-proxy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mov");
        movie(&clip, 640, 360, 3);
        let mut s = effectcraft_host::session();
        s.execute("file.import", json!({"paths": [clip.to_string_lossy()]})).unwrap();
        s.execute("comp.new", json!({"name": "C", "width": 640, "height": 360})).unwrap();

        let spy = Arc::new(Spy(Mutex::new(vec![])));
        let log: LogFn = Arc::new(|_| {});
        let auto = new(spy.clone(), dir.join("proxies"), 1, 1, log);
        let cache = LayerCache::new(1 << 20);
        let comp = s.active_comp_id().unwrap();
        let frame = |scale: f64| {
            auto.render(&RenderJob {
                project: &s.project,
                comp,
                time: Tick::ZERO,
                opts: RenderOpts { scale, ..Default::default() },
                cache: &cache,
                times: None,
            })
            .unwrap();
        };
        frame(0.5);
        // The build runs in the background.
        let t0 = Instant::now();
        while auto.shared.generation.load(Ordering::Relaxed) == 0 {
            assert!(t0.elapsed().as_secs() < 60, "no proxy: {:?}", auto.shared.clips.lock().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        frame(0.5);
        frame(1.0);
        frame(0.25);
        let seen = spy.0.lock().unwrap().clone();
        assert_eq!(seen[0], None, "nothing ready on the first frame");
        let px = seen[1].clone().unwrap();
        assert!(px.ends_with("-half2.mov") && Path::new(&px).is_file(), "{px}");
        assert_eq!(seen[2], None, "Full resolution reads the original");
        assert_eq!(seen[3].as_deref(), Some(px.as_str()));
        // A second server (a restart) reuses the proxy by hash, without building it again.
        let again = new(spy.clone(), dir.join("proxies"), 1, 1, Arc::new(|_| {}));
        let r = make(&again.shared, &clip.to_string_lossy(), 1);
        assert!(matches!(&r, State::Ready(r) if r.path == px && (r.width, r.height) == (320, 180)), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_big_prores_movies_without_their_own_proxy() {
        assert!(may_use(0.5, ProxyUse::CurrentSettings) && may_use(0.25, ProxyUse::UseAll));
        assert!(!may_use(1.0, ProxyUse::CurrentSettings) && !may_use(0.75, ProxyUse::CurrentSettings));
        assert!(!may_use(0.5, ProxyUse::UseNone) && !may_use(0.5, ProxyUse::UseCompOnly));
        let dir = std::env::temp_dir().join(format!("ec-auto-proxy-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.mov");
        movie(&path, 640, 360, 1);
        let mut s = effectcraft_host::session();
        s.execute("file.import", json!({"paths": [path.to_string_lossy()]})).unwrap();
        let f = s.project.items.values().find_map(|it| if let ItemKind::Footage(f) = &it.kind { Some(f.clone()) } else { None }).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(candidate(&f, false, 1), Some(size), "{f:?}");
        assert_eq!(candidate(&f, true, 1), None, "the item has a proxy of its own");
        assert_eq!(candidate(&f, false, size + 1), None, "too small");
        assert_eq!(candidate(&Footage { codec: "H.264".into(), ..f.clone() }, false, 1), None);
        assert_eq!(candidate(&Footage { width: 320, ..f.clone() }, false, 1), None, "already small");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
