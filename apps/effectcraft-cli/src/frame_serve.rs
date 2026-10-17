//! `serve --frames PORT`: the cloud frame server next to the host's control channel, rendering
//! on the GPU with `--gpu` (`plan/cloud-render.md` CR2/CR4); `frames-bench`: a client that
//! measures it over the tunnel (round trips, scrub latency, a soak of random requests).

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use effectcraft_automation::frame_server::{self, CpuRender, FrameConfig, FrameHandle, FrameRender, RenderJob, Rendered};
use effectcraft_engine::remote::{FrameMsg, FrameReply, Mirror};
use effectcraft_engine::remote_encode::{self, Codec, Want};
use effectcraft_engine::remote_wire::{Backoff, Down, Event, FrameClient, Up};
use effectcraft_engine::render::{Accelerator, Backend, ExprHost, FootageSource, LayerCache, RenderOpts, Renderer};
use effectcraft_engine::time::Tick;
use effectcraft_gpu::Gpu;
use serde_json::{Value, json};

use crate::{Args, Failure};

/// Frames on the GPU, read back as the viewer's display RGBA8 (as the desktop app shows them); a
/// frame the GPU can't finish (out of memory, a retired device, a CPU-only step) renders on the
/// CPU instead.
///
/// The card is shared (the desktop on it, other processes), so GPU frames take a slot: at most
/// [`GpuSlots`] render on the GPU at once. An out-of-memory halves the slots (they grow back one
/// at a time after [`REGROW_AFTER`] good frames), and a retired device is rebuilt, at most every
/// [`REVIVE_EVERY`], instead of rendering on the CPU until a restart.
struct GpuRender {
    gpu: std::sync::RwLock<Arc<Gpu>>,
    slots: GpuSlots,
    revived: std::sync::Mutex<Option<Instant>>,
    footage: Arc<dyn FootageSource>,
    expr: Option<Arc<dyn ExprHost>>,
    log: frame_server::LogFn,
    /// Declines logged (once per comp).
    declined: std::sync::Mutex<std::collections::HashSet<u64>>,
}

/// Good GPU frames after which one more slot opens again.
const REGROW_AFTER: u32 = 200;
/// Least time between two device rebuilds.
const REVIVE_EVERY: Duration = Duration::from_secs(5);

/// Frames rendering on the GPU at once.
struct GpuSlots {
    state: std::sync::Mutex<Slots>,
    cv: std::sync::Condvar,
}

struct Slots {
    in_use: usize,
    limit: usize,
    /// The limit never grows past this again: one below a limit that ran out of memory.
    ceiling: usize,
    /// Good frames since the limit last changed.
    good: u32,
}

impl GpuSlots {
    fn new(max: usize) -> GpuSlots {
        let max = max.max(1);
        GpuSlots { state: std::sync::Mutex::new(Slots { in_use: 0, limit: max, ceiling: max, good: 0 }), cv: std::sync::Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn acquire(&self) {
        let mut s = self.lock();
        while s.in_use >= s.limit {
            s = self.cv.wait(s).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        s.in_use += 1;
    }

    /// Give the slot back; `Some(true)` after a good frame, `Some(false)` after an out-of-memory
    /// (the limit halves and never comes back to where it failed). Returns the new limit when it
    /// changed.
    fn release(&self, ok: Option<bool>) -> Option<usize> {
        let mut s = self.lock();
        s.in_use = s.in_use.saturating_sub(1);
        let before = s.limit;
        match ok {
            Some(false) => {
                s.ceiling = s.ceiling.min(s.limit.saturating_sub(1)).max(1);
                s.limit = (s.limit / 2).max(1);
                s.good = 0;
            }
            Some(true) if s.limit < s.ceiling => {
                s.good += 1;
                if s.good >= REGROW_AFTER {
                    s.limit += 1;
                    s.good = 0;
                }
            }
            _ => {}
        }
        let changed = (s.limit != before).then_some(s.limit);
        drop(s);
        self.cv.notify_all();
        changed
    }
}

impl GpuRender {
    fn current(&self) -> Arc<Gpu> {
        self.gpu.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// The device is retired: build a new one (not more often than [`REVIVE_EVERY`]). The layer
    /// cache drops what it held on the old one.
    fn revive(&self, old: &Arc<Gpu>, cache: &LayerCache) -> Option<Arc<Gpu>> {
        let mut at = self.revived.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = self.current();
        if !Arc::ptr_eq(&now, old) {
            return Some(now);
        }
        if at.is_some_and(|t| t.elapsed() < REVIVE_EVERY) {
            return None;
        }
        *at = Some(Instant::now());
        match Gpu::try_headless() {
            Ok(g) => {
                g.context().set_cache_budget(cache.budget());
                let g = Arc::new(g);
                cache.clear();
                *self.gpu.write().unwrap_or_else(std::sync::PoisonError::into_inner) = g.clone();
                (self.log)("the GPU device was lost; rebuilt it");
                Some(g)
            }
            Err(e) => {
                (self.log)(&format!("the GPU device was lost and can't be rebuilt yet ({e}); frames render on the CPU"));
                None
            }
        }
    }
}

impl FrameRender for GpuRender {
    fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        let mut keep_cache = true;
        let mut gpu = Some(self.current());
        if let Some(g) = gpu.clone()
            && g.context().check_health().is_err()
        {
            gpu = self.revive(&g, job.cache);
        }
        if let Some(gpu) = gpu {
            let mut r = Renderer::new(job.project, self.footage.as_ref(), job.opts);
            r.expr = self.expr.as_deref();
            r.cache = Some(job.cache);
            r.times = job.times;
            r.accel = Some(gpu.as_ref() as &dyn Accelerator);
            // Gpu, or Auto with the project on Mercury GPU Acceleration (as the viewer decides).
            if r.active_accel().is_some() {
                self.slots.acquire();
                let out = gpu.within_memory(|| gpu.render_display(&r, job.comp, job.time).and_then(|f| gpu.read_display(&f).map(|b| (f.width, f.height, b))));
                let oom = matches!(&out, Err(e) if e.to_ascii_lowercase().contains("out of memory"));
                let good = matches!(out, Ok(Some(_)));
                if let Some(n) = self.slots.release(if oom {
                    Some(false)
                } else if good {
                    Some(true)
                } else {
                    None
                }) {
                    (self.log)(&format!("{n} frames on the GPU at once now"));
                }
                match out {
                    Ok(Some((width, height, bytes))) => return Ok((Rendered::Rgba8 { width, height, bytes }, true)),
                    Ok(None) => {
                        if self.declined.lock().is_ok_and(|mut d| d.insert(job.comp.0)) {
                            (self.log)(&format!(
                                "the GPU compositor declined comp {}; its frames render on the CPU (RUST_LOG=effectcraft_gpu=debug says why)",
                                job.comp.0
                            ));
                        }
                    }
                    Err(e) => {
                        keep_cache = false;
                        (self.log)(&format!("GPU frame failed ({e}); rendering it on the CPU"));
                    }
                }
            }
        }
        // After a failed GPU attempt the layer cache may hold its partial entries: not used then.
        let mut r = Renderer::new(job.project, self.footage.as_ref(), RenderOpts { backend: Backend::Cpu, ..job.opts });
        r.expr = self.expr.as_deref();
        r.cache = keep_cache.then_some(job.cache);
        // A GPU attempt's times are not this frame's.
        if let Some(t) = job.times {
            t.clear();
        }
        r.times = job.times;
        Ok((Rendered::Image(r.comp_frame_cpu(job.comp, job.time)), false))
    }

    fn gpu_name(&self) -> Option<String> {
        Some(self.current().name())
    }

    fn warm(&self, project: &effectcraft_engine::project::Project, comp: effectcraft_engine::project::ItemId, time: Tick) {
        let gpu = self.current();
        if gpu.context().check_health().is_ok() {
            let r = gpu.warm_project(project, comp, time);
            (self.log)(&format!("GPU warm-up: {} pipelines created, {} sequences ready", r.created, r.ready_sequences));
        }
    }
}

/// Frames on the GPU at once: `EFFECTCRAFT_FRAME_GPU_SLOTS`, else 1 (at most the workers). A
/// heavy 1080p comp needs gigabytes of working textures per frame (Elias's 10-10 Edit: 6–8 GB at
/// peak on the 3090, where two at once ran out of memory), and the card is shared; one GPU frame
/// takes ~6 ms, so scrubbing doesn't wait on it.
pub(crate) fn gpu_slots(threads: usize) -> usize {
    std::env::var("EFFECTCRAFT_FRAME_GPU_SLOTS").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| *n >= 1).unwrap_or(1).min(threads)
}

/// The layer cache's budget (CPU and GPU pixels, also capping the GPU's retained uploads and
/// texture pool): `EFFECTCRAFT_FRAME_CACHE_MB`, else 1024 MiB.
pub(crate) fn cache_budget() -> usize {
    std::env::var("EFFECTCRAFT_FRAME_CACHE_MB").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1024).saturating_mul(1 << 20)
}

/// Frame workers: 8, capped by `--frame-threads` / `EFFECTCRAFT_FRAME_THREADS` (CR1's
/// [`effectcraft_render::concurrency`]).
pub(crate) fn threads() -> usize {
    effectcraft_render::concurrency::frame_threads(frame_server::DEFAULT_THREADS)
}

/// Start the frame server for `serve`. `gpu`: the device the host shares; `control_port`: the
/// host's control port once it listens (control requests over the frame channel go there).
pub fn start(args: &Args, port: u16, gpu: Option<Arc<Gpu>>, control_port: Arc<OnceLock<u16>>) -> Result<FrameHandle, Failure> {
    // Engine and GPU warnings (an out-of-memory, a declined frame) go to standard error too.
    effectcraft_engine::logging::install();
    let t0 = Instant::now();
    let log: frame_server::LogFn = Arc::new(move |s: &str| {
        let _ = writeln!(std::io::stderr(), "[{:9.3}] {s}", t0.elapsed().as_secs_f64());
    });
    let mut s = effectcraft_host::session();
    let preload = match (&args.project, args.flag("--demo")) {
        (Some(p), _) => {
            s.execute("file.open", json!({"path": p})).map_err(|e| Failure::Error(format!("--frames: {e}")))?;
            Some(serde_json::to_string(&*s.project).map_err(|e| Failure::Error(e.to_string()))?)
        }
        (None, true) => {
            s.execute("file.openDemoProject", json!({})).map_err(|e| Failure::Error(e.to_string()))?;
            Some(serde_json::to_string(&*s.project).map_err(|e| Failure::Error(e.to_string()))?)
        }
        _ => None,
    };
    let threads = threads();
    let cache = Arc::new(LayerCache::new(cache_budget()));
    if let Some(g) = &gpu {
        g.context().set_cache_budget(cache.budget());
    }
    let render = renderer(&s, gpu, threads, &log);
    // Never colour bars or Inter from the cloud: frames fail while footage or fonts are missing (CR7).
    let render = crate::asset_guard::wrap(render, log.clone());
    let (token, stored) = frame_server::frame_token(effectcraft_host::config_dir().as_deref(), args.opt("--token")).map_err(Failure::Error)?;
    let control: frame_server::ControlFn = Arc::new(move |method: &str, params: Value| match control_port.get() {
        Some(&p) => effectcraft_automation::host_server::request(p, method, params).unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()})),
        None => json!({"ok": false, "error": "the host is still starting"}),
    });
    let gpu_name = render.gpu_name();
    let handle = frame_server::spawn(FrameConfig { port, token, threads, render, cache, preload: preload.map(Arc::new), control: Some(control), log })
        .map_err(|e| Failure::Error(format!("--frames: {e}")))?;
    let where_ = stored.map(|p| format!("token in {}", p.display())).unwrap_or_else(|| "token from --token / EFFECTCRAFT_FRAME_TOKEN".into());
    let on = match &gpu_name {
        Some(g) => format!("{g}, {} at once on the GPU", gpu_slots(threads)),
        None => "the CPU".into(),
    };
    println!("EffectCraft frames on 127.0.0.1:{} ({where_}; {threads} workers on {on}; layer cache {} MiB)", handle.port, cache_budget() >> 20);
    Ok(handle)
}

/// The server's renderer: [`GpuRender`] on `gpu` (at most [`gpu_slots`] frames on it at once), else
/// the CPU.
pub(crate) fn renderer(s: &effectcraft_engine::Session, gpu: Option<Arc<Gpu>>, threads: usize, log: &frame_server::LogFn) -> Arc<dyn FrameRender> {
    match gpu {
        Some(gpu) => Arc::new(GpuRender {
            gpu: std::sync::RwLock::new(gpu),
            slots: GpuSlots::new(gpu_slots(threads)),
            revived: Default::default(),
            footage: s.footage.clone(),
            expr: s.expr.clone(),
            log: log.clone(),
            declined: Default::default(),
        }),
        None => Arc::new(CpuRender { footage: s.footage.clone(), expr: s.expr.clone() }),
    }
}

/// A frame request's outcome, as the bench sees it.
struct Got {
    latency: Duration,
    server_ms: f64,
    bytes: usize,
    decode_ms: f64,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// `frames-bench --frames ADDR [--token T] [F.ecproj | --demo] [--comp ID] [--scale S]
/// [--max-side PX] [--jpeg-quality Q] [--n N] [--duration SECS]`.
pub fn bench(args: &Args) -> Result<(), Failure> {
    let addr = args.opt("--frames").ok_or_else(|| Failure::Usage("frames-bench needs --frames HOST:PORT".into()))?;
    let addr = if addr.parse::<u16>().is_ok() { format!("127.0.0.1:{addr}") } else { addr.to_string() };
    let (token, _) = frame_server::frame_token(effectcraft_host::config_dir().as_deref(), args.opt("--token")).map_err(Failure::Error)?;
    let mut s = effectcraft_host::session();
    match &args.project {
        Some(p) => s.execute("file.open", json!({"path": p})),
        None => s.execute("file.openDemoProject", json!({})),
    }
    .map_err(|e| Failure::Error(e.to_string()))?;
    let comp = match args.opt("--comp") {
        Some(c) => effectcraft_engine::project::ItemId(c.parse().map_err(|_| Failure::Usage(format!("--comp: `{c}` is not an id")))?),
        None => s.active_comp_id().ok_or_else(|| Failure::Error("the project has no composition".into()))?,
    };
    let duration = s.project.comp(comp).map(|c| c.duration).ok_or_else(|| Failure::Error(format!("no composition {}", comp.0)))?;
    let scale = args.num("--scale").map_err(Failure::Usage)?.unwrap_or(1.0);
    let max_side = args.num("--max-side").map_err(Failure::Usage)?.map(|m| m as u32);
    let quality = args.num("--jpeg-quality").map_err(Failure::Usage)?.unwrap_or(85.0) as u8;
    let n = args.num("--n").map_err(Failure::Usage)?.unwrap_or(30.0) as usize;
    let soak = args.num("--duration").map_err(Failure::Usage)?.unwrap_or(0.0);

    let c = FrameClient::connect(&addr, &token, "frames-bench", Backoff::default());
    let wait = |c: &FrameClient, secs: u64, f: &mut dyn FnMut(Event) -> Option<Result<(), String>>| -> Result<(), Failure> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(ev) = c.recv(Duration::from_millis(100))
                && let Some(r) = f(ev)
            {
                return r.map_err(Failure::Error);
            }
        }
        Err(Failure::Error("timed out".into()))
    };
    wait(&c, 30, &mut |ev| match ev {
        Event::Connected { welcome } => {
            println!("connected: {}", serde_json::to_string(&welcome).unwrap_or_default());
            Some(Ok(()))
        }
        Event::Refused { error } => Some(Err(format!("refused: {error}"))),
        Event::Disconnected { error } => {
            println!("waiting for {addr}: {error}");
            None
        }
        Event::Down(..) => None,
    })?;

    // Round trips: small echoes (RTT), then 1 MB.
    let echo = |c: &FrameClient, id: u64, bytes: Vec<u8>| -> Result<Duration, Failure> {
        let t = Instant::now();
        let len = bytes.len();
        if !c.send(Up::Echo { id }, bytes) {
            return Err(Failure::Error("not connected".into()));
        }
        let mut wait_echo = |ev| match ev {
            Event::Down(Down::Echo { id: got }, p) if got == id => Some(if p.len() == len { Ok(()) } else { Err("echo damaged".into()) }),
            Event::Disconnected { error } => Some(Err(error)),
            _ => None,
        };
        wait(c, 60, &mut wait_echo)?;
        Ok(t.elapsed())
    };
    let mut rtts: Vec<f64> = vec![];
    for i in 0..20 {
        rtts.push(echo(&c, i, vec![0; 16])?.as_secs_f64() * 1000.0);
    }
    let rtt = pct(&mut rtts, 0.5);
    println!("RTT: p50 {rtt:.1} ms, p90 {:.1} ms", pct(&mut rtts, 0.9));
    for i in 0..3 {
        let mb: Vec<u8> = (0..(1u32 << 20)).map(|k| (k * 31 % 251) as u8).collect();
        let d = echo(&c, 100 + i, mb)?;
        println!("1 MB round trip: {:.0} ms ({:.1} MB/s both ways)", d.as_secs_f64() * 1000.0, 2.0 / d.as_secs_f64());
    }

    // The project, as the app would send it.
    let mut mirror = Mirror::default();
    let t = Instant::now();
    if let Some(msg) = mirror.sync(s.revision, &s.project) {
        let bytes = serde_json::to_vec(&msg).map(|v| v.len()).unwrap_or(0);
        if !c.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]) {
            return Err(Failure::Error("not connected".into()));
        }
        println!("project sent: {} KB in {:.0} ms", bytes / 1024, t.elapsed().as_secs_f64() * 1000.0);
    }
    let revision = s.revision;
    let want = Want { codec: Codec::Jpeg, quality, max_size: max_side.map(|m| [m, m]), refine: false };
    // As the viewer asks: the GPU when the project renders with Mercury GPU Acceleration.
    let opts = RenderOpts { scale, backend: Backend::Auto, ..Default::default() };
    let next_id = AtomicU64::new(1);
    let mut seed = Instant::now().elapsed().subsec_nanos() as u64 | 1;
    let mut random_time = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        Tick(((seed % 1_000_000) as i64).saturating_mul(duration.0.max(1)) / 1_000_000)
    };
    let request = |c: &FrameClient, t: Tick, urgent: bool| -> u64 {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let msg = FrameMsg::Render { id, revision, comp, time: t, opts: Box::new(opts), disk: None, layers: false, prefetch: vec![] };
        c.send(Up::Msg { msg, want, urgent }, vec![]);
        id
    };
    // One reply: (id, outcome or failure).
    let take = |ev: Event, sent: &std::collections::HashMap<u64, Instant>| -> Option<(u64, Result<Got, String>)> {
        match ev {
            Event::Down(Down::Reply { reply: FrameReply::Frame { id, ms, .. }, pixels: Some(px) }, payload) => {
                let latency = sent.get(&id)?.elapsed();
                let t = Instant::now();
                let ok = remote_encode::decode(&px, &payload).map(|_| ());
                let decode_ms = t.elapsed().as_secs_f64() * 1000.0;
                Some((id, ok.map(|()| Got { latency, server_ms: ms, bytes: payload.len(), decode_ms })))
            }
            Event::Down(Down::Reply { reply: FrameReply::Failed { id, error, .. }, .. }, _) => Some((id, Err(error))),
            Event::Down(Down::Reply { reply: FrameReply::Resync { error }, .. }, _) => Some((0, Err(format!("resync: {error}")))),
            Event::Disconnected { error } => Some((0, Err(format!("disconnected: {error}")))),
            _ => None,
        }
    };

    // Scrubbing: one urgent frame at a time, a random time each.
    let (mut lat, mut over, mut srv, mut kb, mut dec) = (vec![], vec![], vec![], vec![], vec![]);
    for i in 0..n {
        let mut sent = std::collections::HashMap::new();
        let id = request(&c, random_time(), true);
        sent.insert(id, Instant::now());
        let mut got = None;
        let mut on_ev = |ev| {
            let (rid, r) = take(ev, &sent)?;
            (rid == id || rid == 0).then(|| {
                got = Some(r);
                Ok(())
            })
        };
        wait(&c, 120, &mut on_ev)?;
        match got {
            Some(Ok(g)) => {
                let l = g.latency.as_secs_f64() * 1000.0;
                if i > 0 {
                    // The first frame warms up (pipelines, footage); it is reported, not counted.
                    lat.push(l);
                    over.push(l - rtt);
                    srv.push(g.server_ms);
                    kb.push(g.bytes as f64 / 1024.0);
                    dec.push(g.decode_ms);
                } else {
                    println!("first frame: {l:.0} ms (render {:.0} ms, {} KB)", g.server_ms, g.bytes / 1024);
                }
            }
            Some(Err(e)) => return Err(Failure::Error(format!("frame {id}: {e}"))),
            None => {}
        }
    }
    println!(
        "scrub ({n} frames, scale {scale}{}): latency p50 {:.0} / p90 {:.0} / max {:.0} ms; over RTT p50 {:.0} / p90 {:.0} ms; server render p50 {:.0} ms; {:.0} KB p50; decode p50 {:.1} ms",
        max_side.map(|m| format!(", max side {m}")).unwrap_or_default(),
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.9),
        pct(&mut lat, 1.0),
        pct(&mut over, 0.5),
        pct(&mut over, 0.9),
        pct(&mut srv, 0.5),
        pct(&mut kb, 0.5),
        pct(&mut dec, 0.5),
    );

    // Soak: random urgent and prefetch requests, some cancelled, for `--duration` seconds.
    if soak > 0.0 {
        let end = Instant::now() + Duration::from_secs_f64(soak);
        let mut sent: std::collections::HashMap<u64, Instant> = std::collections::HashMap::new();
        let (mut done, mut failed, mut cancelled, mut reconnects) = (0u64, 0u64, 0u64, 0u32);
        let mut lat: Vec<f64> = vec![];
        let mut report = Instant::now();
        let mut k = 0u64;
        while Instant::now() < end {
            while sent.len() < 6 {
                k += 1;
                let id = request(&c, random_time(), k.is_multiple_of(3));
                sent.insert(id, Instant::now());
                if k.is_multiple_of(7) {
                    c.send(Up::Cancel { ids: vec![id] }, vec![]);
                    sent.remove(&id);
                    cancelled += 1;
                }
            }
            match c.recv(Duration::from_millis(200)) {
                // The tunnel went: wait for the client to reconnect, then resend the project, as
                // the app does.
                Some(Event::Disconnected { error }) => {
                    println!("connection lost ({error}); waiting for it to come back");
                    let lost = Instant::now();
                    wait(&c, 600, &mut |ev| match ev {
                        Event::Connected { .. } => Some(Ok(())),
                        Event::Refused { error } => Some(Err(format!("refused: {error}"))),
                        _ => None,
                    })?;
                    mirror.reset();
                    if let Some(msg) = mirror.sync(s.revision, &s.project) {
                        c.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]);
                    }
                    reconnects += 1;
                    sent.clear();
                    println!("reconnected after {:.1} s; project sent again", lost.elapsed().as_secs_f64());
                }
                Some(ev) => {
                    if let Some((id, r)) = take(ev, &sent) {
                        sent.remove(&id);
                        match r {
                            Ok(g) => {
                                done += 1;
                                lat.push(g.latency.as_secs_f64() * 1000.0);
                            }
                            Err(e) if id == 0 => return Err(Failure::Error(e)),
                            Err(_) => failed += 1,
                        }
                    }
                }
                None => {}
            }
            // Requests that never come back (cancelled while rendering) are forgotten.
            sent.retain(|_, t| t.elapsed() < Duration::from_secs(60));
            if report.elapsed() > Duration::from_secs(30) {
                report = Instant::now();
                println!(
                    "soak: {done} frames, {failed} failed, {cancelled} cancelled; latency p50 {:.0} / p90 {:.0} ms",
                    pct(&mut lat, 0.5),
                    pct(&mut lat, 0.9)
                );
                lat.clear();
            }
        }
        println!("soak done: {done} frames, {failed} failed, {cancelled} cancelled, {reconnects} reconnects");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_slots_halve_on_out_of_memory_and_grow_back_below_it() {
        let s = GpuSlots::new(4);
        s.acquire();
        s.acquire();
        assert_eq!(s.release(Some(false)), Some(2));
        assert_eq!(s.release(None), None);
        assert_eq!(GpuSlots::new(1).release(Some(false)), None, "never below one");
        // Grows back one at a time, but not to 4 again (that ran out of memory): 3 at most.
        let mut limits = vec![];
        for _ in 0..(4 * REGROW_AFTER) {
            s.acquire();
            if let Some(n) = s.release(Some(true)) {
                limits.push(n);
            }
        }
        assert_eq!(limits, vec![3]);
        // A declined frame changes nothing.
        s.acquire();
        assert_eq!(s.release(None), None);
        // Out of memory at 3: down to 1, and 2 at most from then on.
        s.acquire();
        assert_eq!(s.release(Some(false)), Some(1));
        let mut limits = vec![];
        for _ in 0..(4 * REGROW_AFTER) {
            s.acquire();
            if let Some(n) = s.release(Some(true)) {
                limits.push(n);
            }
        }
        assert_eq!(limits, vec![2]);
    }
}
