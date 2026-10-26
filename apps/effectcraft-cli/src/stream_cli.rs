//! The playback stream from the command line (`plan/cloud-render.md` CR6a): `stream-bench`
//! measures rendering and H.264 chunk encoding of a comp on this machine; `stream-test` plays a
//! comp from a frame server's stream and reports each chunk's arrival against its deadline.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use effectcraft_automation::frame_server::{FrameRender, RenderJob, Rendered};
use effectcraft_engine::project::{ItemId, ItemKind, Project};
use effectcraft_engine::remote_encode;
use effectcraft_engine::render::{Backend, LayerCache, RenderOpts};
use effectcraft_engine::time::Tick;
use effectcraft_media::preview_stream::{ChunkPreset, ChunkSpec, decode_chunk, encode_chunk, even};
use serde_json::json;

use crate::{Args, Failure};

/// `--comp`: an id, or a composition's name; else the session's active comp.
pub(crate) fn find_comp(p: &Project, arg: Option<&str>, active: Option<ItemId>) -> Result<ItemId, Failure> {
    let Some(a) = arg else { return active.ok_or_else(|| Failure::Error("the project has no composition".into())) };
    if let Ok(n) = a.parse::<u64>()
        && p.comp(ItemId(n)).is_some()
    {
        return Ok(ItemId(n));
    }
    p.items
        .values()
        .find(|it| matches!(it.kind, ItemKind::Comp(_)) && it.name == a)
        .map(|it| it.id)
        .ok_or_else(|| Failure::Error(format!("no composition `{a}`")))
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// One rendered viewer frame as premultiplied RGBA8.
pub(crate) fn rgba8(r: Rendered) -> (u32, u32, Vec<u8>) {
    match r {
        Rendered::Image(img) => (img.width, img.height, remote_encode::rgba8_premultiplied_par(&img)),
        Rendered::Rgba8 { width, height, bytes } => (width, height, bytes),
    }
}

/// `stream-bench F.ecproj [--comp C] [--start SECS] [--n FRAMES] [--gpu] [--scale S] [--proxies]`: render
/// `n` consecutive frames, then encode them as H.264 chunks every way the stream could.
pub fn bench(args: &Args) -> Result<(), Failure> {
    let mut s = effectcraft_host::session();
    match &args.project {
        Some(p) => s.execute("file.open", json!({"path": p})),
        None => s.execute("file.openDemoProject", json!({})),
    }
    .map_err(|e| Failure::Error(e.to_string()))?;
    let comp = find_comp(&s.project, args.opt("--comp"), s.active_comp_id())?;
    let c = s.project.comp(comp).ok_or_else(|| Failure::Error("no comp".into()))?.clone();
    let rate = c.frame_rate;
    let start = Tick::from_seconds_f64(args.num("--start").map_err(Failure::Usage)?.unwrap_or(30.0));
    let n = args.num("--n").map_err(Failure::Usage)?.unwrap_or(90.0).max(1.0) as usize;
    let scale = args.num("--scale").map_err(Failure::Usage)?.unwrap_or(1.0);
    let gpu = match args.flag("--gpu") {
        true => Some(Arc::new(effectcraft_gpu::Gpu::try_headless().map_err(|e| Failure::Error(format!("--gpu: {e}")))?)),
        false => None,
    };
    let log: effectcraft_automation::frame_server::LogFn = Arc::new(|l: &str| eprintln!("{l}"));
    let threads = crate::frame_serve::threads();
    let cache = LayerCache::new(crate::frame_serve::cache_budget());
    if let Some(g) = &gpu {
        g.context().set_cache_budget(cache.budget());
    }
    let render: Arc<dyn FrameRender> = crate::frame_serve::renderer(&s, gpu, threads, &log);
    // `--proxies`: as the frame server renders, with its half-size proxies of big clips.
    let render = match args.flag("--proxies") {
        true => crate::auto_proxy::wrap_ready(render, &s.project, std::time::Duration::from_secs(7200), log.clone()),
        false => render,
    };
    let project = s.project.clone();
    let first = rate.frame_at(start);
    let opts = RenderOpts { scale, backend: Backend::Auto, ..Default::default() };
    println!(
        "stream-bench: comp {} ({}×{} at {}/{} fps), {n} frames from {:.2} s, scale {scale}, {threads} workers, GPU {}",
        comp.0,
        c.width,
        c.height,
        rate.num,
        rate.den,
        start.seconds(),
        render.gpu_name().unwrap_or_else(|| "off".into())
    );

    // Render: `threads` workers take frames in play order.
    let render_only = args.flag("--render-only");
    let frames: Vec<Mutex<Option<(u32, u32, Vec<u8>)>>> = (0..n).map(|_| Mutex::new(None)).collect();
    // When each frame started and ended (seconds from the start), for the per-second report.
    let spans: Vec<Mutex<(f64, f64)>> = (0..n).map(|_| Mutex::new((0.0, 0.0))).collect();
    let next = AtomicUsize::new(0);
    let warm = Instant::now();
    if let Ok((r, _)) = render.render(&RenderJob { project: &project, comp, time: rate.tick_of(first), opts, cache: &cache, times: None }) {
        drop(r);
    }
    println!("render: first (cold) frame {:.0} ms", ms(warm));
    let t0 = Instant::now();
    let gpu_frames = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let time = rate.tick_of(first + i as i64);
                    let began = t0.elapsed().as_secs_f64();
                    let out = render.render(&RenderJob { project: &project, comp, time, opts, cache: &cache, times: None });
                    if let Some(sp) = spans.get(i)
                        && let Ok(mut g) = sp.lock()
                    {
                        *g = (began, t0.elapsed().as_secs_f64());
                    }
                    match out {
                        Ok((r, gpu)) => {
                            if gpu {
                                gpu_frames.fetch_add(1, Ordering::Relaxed);
                            }
                            if render_only {
                                drop(r);
                            } else if let Some(slot) = frames.get(i)
                                && let Ok(mut g) = slot.lock()
                            {
                                *g = Some(rgba8(r));
                            }
                        }
                        Err(e) => eprintln!("frame {i}: {e}"),
                    }
                }
            });
        }
    });
    let render_s = t0.elapsed().as_secs_f64();
    if render_only {
        // Per second of comp time: frames / (last end − first start).
        let spans: Vec<(f64, f64)> = spans.into_iter().map(|m| m.into_inner().unwrap_or((0.0, 0.0))).collect();
        let per = rate.as_f64().round().max(1.0) as usize;
        let mut line = String::new();
        let mut slow = vec![];
        for (k, sec) in spans.chunks(per).enumerate() {
            let a = sec.iter().map(|s| s.0).fold(f64::INFINITY, f64::min);
            let b = sec.iter().map(|s| s.1).fold(0.0, f64::max);
            let fps = sec.len() as f64 / (b - a).max(1e-6);
            let at = (first as f64 + (k * per) as f64) / rate.as_f64();
            line.push_str(&format!(" {at:.0}s:{fps:.0}"));
            if fps < rate.as_f64() {
                slow.push(format!("{at:.0}"));
            }
        }
        println!("render: {n} frames in {render_s:.2} s = {:.1} fps; fps per comp second:{line}", n as f64 / render_s);
        println!("below real time at: {}", if slow.is_empty() { "none".to_string() } else { slow.join(", ") });
        return Ok(());
    }
    let frames: Vec<(u32, u32, Vec<u8>)> = frames.into_iter().filter_map(|m| m.into_inner().ok().flatten()).collect();
    let Some((w, h, _)) = frames.first().map(|f| (f.0, f.1, ())) else { return Err(Failure::Error("no frame rendered".into())) };
    println!(
        "render: {} frames {w}×{h} in {:.2} s = {:.1} fps ({} on the GPU)",
        frames.len(),
        render_s,
        frames.len() as f64 / render_s,
        gpu_frames.load(Ordering::Relaxed)
    );
    let fps = rate.as_f64();
    let bg = c.background;
    let sizes = [(w, h, "full"), (w / 2, h / 2, "half")];
    for (sw, sh, label) in sizes {
        let scaled: Vec<Vec<u8>> = if (sw, sh) == (w, h) {
            frames.iter().map(|f| f.2.clone()).collect()
        } else {
            frames.iter().map(|f| remote_encode::fit(f.2.clone(), f.0, f.1, [sw, sh]).2).collect()
        };
        let (fw, fh) = remote_encode::fit_size(w, h, [sw, sh]);
        let refs: Vec<&[u8]> = scaled.iter().map(Vec::as_slice).collect();
        let (ew, eh) = (even(fw), even(fh));
        let chunk = 15.min(refs.len());
        // One chunk on one thread, per preset and quality.
        for (preset, crf) in [(ChunkPreset::Speed, 23.0), (ChunkPreset::Speed, 28.0), (ChunkPreset::Balanced, 23.0)] {
            let spec = ChunkSpec { crf, preset, ..ChunkSpec::new(ew, eh, rate.num as u32, rate.den as u32) };
            let t = Instant::now();
            let bytes = encode_chunk(refs.get(..chunk).unwrap_or(&[]), fw, fh, bg, &spec).map_err(Failure::Error)?;
            let per = ms(t) / chunk as f64;
            let kbps = bytes.len() as f64 * 8.0 / (chunk as f64 / fps) / 1000.0;
            let t = Instant::now();
            let back = decode_chunk(&bytes, 1).map_err(Failure::Error)?;
            let dec1 = ms(t) / back.len().max(1) as f64;
            let psnr = psnr(refs.get(..chunk.min(back.len())).unwrap_or(&[]), &back.iter().map(|f| f.rgba.as_slice()).collect::<Vec<_>>(), bg);
            println!(
                "encode {label} {ew}×{eh} {preset:?} crf {crf}: 1 thread {per:.1} ms/frame ({:.1} fps), {kbps:.0} kbit/s, PSNR {psnr:.1} dB; decode 1 thread {dec1:.1} ms/frame",
                1000.0 / per
            );
        }
        // One chunk on several threads: how long the first chunk takes (warm-up).
        for t_n in [4usize, 8, 16] {
            let spec = ChunkSpec { threads: t_n, ..ChunkSpec::new(ew, eh, rate.num as u32, rate.den as u32) };
            let t = Instant::now();
            let bytes = encode_chunk(refs.get(..chunk).unwrap_or(&[]), fw, fh, bg, &spec).map_err(Failure::Error)?;
            let kbps = bytes.len() as f64 * 8.0 / (chunk as f64 / fps) / 1000.0;
            println!("encode {label} Speed crf 23: one {chunk}-frame chunk on {t_n} threads {:.0} ms ({kbps:.0} kbit/s)", ms(t));
        }
        // Many chunks at once, one thread each: throughput.
        let chunks: Vec<&[&[u8]]> = refs.chunks(chunk).collect();
        for par in [4usize, 8, 16, 32] {
            let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(par).build() else { continue };
            let spec = ChunkSpec::new(ew, eh, rate.num as u32, rate.den as u32);
            let t = Instant::now();
            // Every chunk `par / chunks` times over, so `par` encoders are busy at once.
            let reps = par.div_ceil(chunks.len()).max(1);
            let work: Vec<&[&[u8]]> = (0..reps).flat_map(|_| chunks.iter().copied()).collect();
            let total: usize = work.iter().map(|c| c.len()).sum();
            let bytes: usize = pool.install(|| {
                use rayon::prelude::*;
                work.par_iter().map(|c| encode_chunk(c, fw, fh, bg, &spec).map(|b| b.len()).unwrap_or(0)).sum()
            });
            let secs = t.elapsed().as_secs_f64();
            println!(
                "encode {label} Speed crf 23: {} chunks, {par} at once, 1 thread each: {:.1} fps ({:.0} kbit/s)",
                work.len(),
                total as f64 / secs,
                bytes as f64 * 8.0 / (total as f64 / fps) / 1000.0
            );
        }
        // Decoding (the Mac's side, on this CPU) with several threads.
        let spec = ChunkSpec::new(ew, eh, rate.num as u32, rate.den as u32);
        if let Ok(bytes) = encode_chunk(refs.get(..chunk).unwrap_or(&[]), fw, fh, bg, &spec) {
            let t = Instant::now();
            let back = decode_chunk(&bytes, 8).map_err(Failure::Error)?;
            println!("decode {label}: 8 threads {:.1} ms/frame", ms(t) / back.len().max(1) as f64);
        }
    }
    Ok(())
}

/// Mean PSNR of decoded frames against their sources (over `bg`), in dB.
fn psnr(src: &[&[u8]], got: &[&[u8]], bg: [f32; 3]) -> f64 {
    let mut sum = 0.0;
    let mut n = 0usize;
    for (a, b) in src.iter().zip(got) {
        let mut se = 0.0f64;
        let mut k = 0usize;
        for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0.iter()) {
            let rest = 1.0 - p[3] as f64 / 255.0;
            for c in 0..3 {
                let v = p[c] as f64 + bg[c] as f64 * 255.0 * rest;
                let d = v - q[c] as f64;
                se += d * d;
                k += 1;
            }
        }
        if k > 0 {
            let mse = se / k as f64;
            sum += if mse <= 1e-9 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() };
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// A chunk as the test client saw it.
struct Arrival {
    seq: u64,
    revision: u64,
    first: i64,
    frames: u32,
    scale: f32,
    bytes: usize,
    /// Seconds after the start request: arrived, decoded.
    at: f64,
    decoded: f64,
    decode_ms: f64,
}

/// `stream-test --frames ADDR [--token T] [F.ecproj | --demo] [--comp C] [--start SECS]
/// [--duration SECS] [--edit-at SECS] [--scale S] [--max-side PX] [--fixed]`: play a comp from a
/// frame server's playback stream as the app would (playback starts when the first chunk is
/// decoded and waits when a frame is missing), and report each chunk against its deadline, the
/// bitrate and the stalls. `--edit-at`: change a layer at that point of playback (the stream must
/// render again from there).
pub fn test(args: &Args) -> Result<(), Failure> {
    use effectcraft_engine::remote::{FrameMsg, FrameReply, Mirror};
    use effectcraft_engine::remote_encode::Want;
    use effectcraft_engine::remote_stream::{StreamCodec, StreamMsg, StreamReply};
    use effectcraft_engine::remote_wire::{Backoff, Down, Event, FrameClient, Up};
    use std::time::Duration;

    let addr = args.opt("--frames").ok_or_else(|| Failure::Usage("stream-test needs --frames HOST:PORT".into()))?;
    let addr = if addr.parse::<u16>().is_ok() { format!("127.0.0.1:{addr}") } else { addr.to_string() };
    let (token, _) =
        effectcraft_automation::frame_server::frame_token(effectcraft_host::config_dir().as_deref(), args.opt("--token")).map_err(Failure::Error)?;
    let mut s = effectcraft_host::session();
    match &args.project {
        Some(p) => s.execute("file.open", json!({"path": p})),
        None => s.execute("file.openDemoProject", json!({})),
    }
    .map_err(|e| Failure::Error(e.to_string()))?;
    let comp = find_comp(&s.project, args.opt("--comp"), s.active_comp_id())?;
    let c = s.project.comp(comp).ok_or_else(|| Failure::Error("no comp".into()))?.clone();
    let rate = c.frame_rate;
    let fps = rate.as_f64();
    let start = Tick::from_seconds_f64(args.num("--start").map_err(Failure::Usage)?.unwrap_or(0.0));
    let secs = args.num("--duration").map_err(Failure::Usage)?.unwrap_or(30.0);
    let edit_at = args.num("--edit-at").map_err(Failure::Usage)?;
    let scale = args.num("--scale").map_err(Failure::Usage)?.unwrap_or(1.0);
    let max_side = args.num("--max-side").map_err(Failure::Usage)?.map(|m| m as u32);
    let from = rate.frame_at(start);
    let to = (from + (secs * fps).round() as i64).min(rate.frame_at(c.duration));
    let frames_total = (to - from).max(1);

    let client = FrameClient::connect(&addr, &token, "stream-test", Backoff::default());
    let wait_for = |what: &str, secs: u64, f: &mut dyn FnMut(Event) -> Option<Result<(), String>>| -> Result<(), Failure> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(ev) = client.recv(Duration::from_millis(100))
                && let Some(r) = f(ev)
            {
                return r.map_err(Failure::Error);
            }
        }
        Err(Failure::Error(format!("timed out waiting for {what}")))
    };
    wait_for("the server", 60, &mut |ev| match ev {
        Event::Connected { welcome } => {
            println!("connected: {}", serde_json::to_string(&welcome).unwrap_or_default());
            Some(Ok(()))
        }
        Event::Refused { error } => Some(Err(format!("refused: {error}"))),
        _ => None,
    })?;
    // RTT, for the report.
    let t = Instant::now();
    client.send(Up::Echo { id: 1 }, vec![0; 16]);
    wait_for("an echo", 30, &mut |ev| matches!(ev, Event::Down(Down::Echo { id: 1 }, _)).then_some(Ok(())))?;
    let rtt = ms(t);
    // The project, as the app sends it.
    let mut mirror = Mirror::default();
    let t = Instant::now();
    if let Some(msg) = mirror.sync(s.revision, &s.project) {
        client.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]);
    }
    // An echo after it: the project has arrived and been loaded once it comes back.
    client.send(Up::Echo { id: 2 }, vec![]);
    wait_for("the project", 600, &mut |ev| match ev {
        Event::Down(Down::Echo { id: 2 }, _) => Some(Ok(())),
        Event::Down(Down::Reply { reply: FrameReply::Resync { error }, .. }, _) => Some(Err(format!("resync: {error}"))),
        _ => None,
    })?;
    println!("RTT {rtt:.0} ms; project sent and loaded in {:.1} s", t.elapsed().as_secs_f64());

    let stream = 1u64;
    let opts = RenderOpts { scale, backend: Backend::Auto, ..Default::default() };
    let begin = Instant::now();
    let msg = FrameMsg::Stream(StreamMsg::Start {
        stream,
        revision: s.revision,
        comp,
        from: rate.tick_of(from),
        to: Some(rate.tick_of(to)),
        loop_from: None,
        looping: false,
        opts: Box::new(opts),
        codec: StreamCodec::H264,
        crf: 23.0,
        chunk_frames: 15,
        ahead_secs: args.num("--ahead").map_err(Failure::Usage)?.unwrap_or(4.0) as f32,
        adaptive: !args.flag("--fixed"),
        max_size: max_side.map(|m| [m, m]),
        cache: None,
    });
    client.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]);

    // Chunks decode on other threads, as the app would; (seq, decoded at, decode ms).
    let (dec_tx, dec_rx) = std::sync::mpsc::channel::<(u64, f64, f64, Result<usize, String>)>();
    let mut arrivals: Vec<Arrival> = vec![];
    // Frames decoded (first arrival per frame), and the newest revision seen per frame.
    let mut have = vec![false; frames_total as usize];
    let mut play_start: Option<Instant> = None;
    // The playhead: frames shown so far (it waits at a missing frame), and stalls.
    let (mut shown, mut stalled, mut stalls) = (0i64, 0.0f64, 0u32);
    let mut in_stall = false;
    let mut last_tick = Instant::now();
    let mut last_report = Instant::now();
    let (mut ended, mut stale_at, mut edited_at) = (false, None::<f64>, None::<f64>);
    let mut edit_rev = None::<u64>;
    // The newest Stale notice: (revision, first frame): older chunks from there on don't count.
    let mut stale_from: Option<(u64, i64)> = None;
    let mut first_new_chunk = None::<f64>;
    let mut started = None;
    let timeout = Duration::from_secs_f64(secs * 4.0 + 60.0);
    while begin.elapsed() < timeout {
        // Advance the playhead.
        if let Some(ps) = play_start {
            let dt = last_tick.elapsed().as_secs_f64();
            let due = (ps.elapsed().as_secs_f64() - stalled) * fps;
            let want = (due.floor() as i64).min(frames_total);
            while shown < want && have.get(shown as usize).copied().unwrap_or(false) {
                shown += 1;
            }
            if shown < want && shown < frames_total {
                if !in_stall {
                    stalls += 1;
                    in_stall = true;
                }
                stalled += dt;
            } else {
                in_stall = false;
            }
        }
        last_tick = Instant::now();
        if shown >= frames_total {
            break;
        }
        // Report the playhead.
        if last_report.elapsed() > Duration::from_millis(100) {
            last_report = Instant::now();
            let received = arrivals.iter().map(|a| a.seq).max();
            let msg = FrameMsg::Stream(StreamMsg::Playhead { stream, time: rate.tick_of(from + shown), playing: play_start.is_some(), received });
            client.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]);
        }
        // The edit, at its point of playback.
        if let (Some(e), None) = (edit_at, edited_at)
            && play_start.is_some()
            && shown as f64 / fps >= e
        {
            let at = rate.tick_of(from + shown);
            match edit(&mut s, comp, at) {
                Ok(what) => {
                    if let Some(msg) = mirror.sync(s.revision, &s.project) {
                        client.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]);
                    }
                    edited_at = Some(begin.elapsed().as_secs_f64());
                    edit_rev = Some(s.revision);
                    println!("edit at {:.2} s of playback ({what}) → revision {}", shown as f64 / fps, s.revision);
                }
                Err(e) => {
                    println!("edit failed: {e}");
                    edited_at = Some(-1.0);
                }
            }
        }
        while let Ok((seq, done, dms, r)) = dec_rx.try_recv() {
            let Some(a) = arrivals.iter_mut().find(|a| a.seq == seq) else { continue };
            a.decoded = done;
            a.decode_ms = dms;
            match r {
                Ok(n) if n as u32 == a.frames => {
                    let old = stale_from.is_some_and(|(r, f)| a.revision < r && a.first + i64::from(a.frames) > f);
                    for f in a.first..a.first + i64::from(a.frames) {
                        if old {
                            break;
                        }
                        if let Some(h) = have.get_mut((f - from) as usize) {
                            *h = true;
                        }
                    }
                    if play_start.is_none() {
                        play_start = Some(Instant::now());
                        last_tick = Instant::now();
                        println!("warm-up: first chunk decoded {:.0} ms after the start request", done * 1000.0);
                    }
                }
                Ok(n) => println!("chunk #{seq}: {n} frames decoded, {} expected", a.frames),
                Err(e) => println!("chunk #{seq}: {e}"),
            }
        }
        let Some(ev) = client.recv(Duration::from_millis(5)) else { continue };
        match ev {
            Event::Down(Down::Reply { reply: FrameReply::Stream(r), .. }, payload) => match r {
                StreamReply::Started { width, height, chunk_frames, .. } => {
                    started = Some((width, height));
                    println!("stream started: {width}×{height}, {chunk_frames} frames per chunk, {:.0} ms after the request", ms(begin));
                }
                StreamReply::Chunk { seq, revision, first, frames, scale, .. } => {
                    let at = begin.elapsed().as_secs_f64();
                    if edit_rev.is_some_and(|r| revision >= r) && first_new_chunk.is_none() {
                        first_new_chunk = Some(at);
                    }
                    arrivals.push(Arrival { seq, revision, first, frames, scale, bytes: payload.len(), at, decoded: f64::NAN, decode_ms: 0.0 });
                    let tx = dec_tx.clone();
                    std::thread::spawn(move || {
                        let t = Instant::now();
                        let r = decode_chunk(&payload, 4).map(|f| f.len());
                        let _ = tx.send((seq, begin.elapsed().as_secs_f64(), ms(t), r));
                    });
                }
                StreamReply::Stale { revision, from: f, .. } => {
                    stale_at = Some(begin.elapsed().as_secs_f64());
                    println!("stale: revision {revision} from {:.2} s", f.seconds());
                    // Frames from the playhead on that belong to the old revision: wanted again.
                    let lo = rate.frame_at(f).max(from + shown);
                    stale_from = Some((revision, lo));
                    for fr in lo..to {
                        if let Some(h) = have.get_mut((fr - from) as usize) {
                            *h = false;
                        }
                    }
                }
                StreamReply::End { .. } => ended = true,
                StreamReply::Failed { error, .. } => return Err(Failure::Error(format!("stream failed: {error}"))),
            },
            Event::Down(Down::Reply { reply: FrameReply::Resync { error }, .. }, _) => return Err(Failure::Error(format!("resync: {error}"))),
            Event::Disconnected { error } => return Err(Failure::Error(format!("disconnected: {error}"))),
            _ => {}
        }
    }
    client.send(Up::Msg { msg: FrameMsg::Stream(StreamMsg::Stop { stream }), want: Want::default(), urgent: false }, vec![]);
    std::thread::sleep(Duration::from_millis(200));

    // The report: each chunk's first arrival against the moment playback shows its first frame
    // (playback started when the first chunk was decoded; stalls push later frames back).
    let Some(ps) = play_start else { return Err(Failure::Error("no chunk arrived".into())) };
    let ps = ps.duration_since(begin).as_secs_f64();
    let mut firsts: std::collections::BTreeMap<i64, &Arrival> = std::collections::BTreeMap::new();
    for a in &arrivals {
        let key = a.first;
        let after_edit = edit_rev.is_some_and(|r| a.revision >= r);
        match firsts.get(&key) {
            Some(old) if !(after_edit && edit_rev.is_some_and(|r| old.revision < r)) => {}
            _ => {
                firsts.insert(key, a);
            }
        }
    }
    // Deadline = the moment playback shows the chunk's first frame; checked on arrival (the
    // server's part) and once decoded (this machine's part too).
    let (mut late, mut worst, mut slack) = (0u32, f64::NEG_INFINITY, vec![]);
    let (mut late_dec, mut slack_dec) = (0u32, vec![]);
    let mut by_scale: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for (i, a) in firsts.values().enumerate() {
        let deadline = ps + (a.first - from) as f64 / fps;
        let margin = deadline - a.at;
        let margin_dec = deadline - a.decoded;
        *by_scale.entry(format!("{}", a.scale)).or_default() += 1;
        if i > 0 {
            slack.push(margin);
            worst = worst.max(-margin);
            if margin < 0.0 {
                late += 1;
            }
            slack_dec.push(margin_dec);
            if margin_dec < 0.0 {
                late_dec += 1;
            }
        }
    }
    let bytes: usize = arrivals.iter().map(|a| a.bytes).sum();
    let wall = arrivals.iter().map(|a| a.at).fold(0.0, f64::max).max(1e-3);
    let mut dec: Vec<f64> = arrivals.iter().map(|a| a.decode_ms / f64::from(a.frames.max(1))).collect();
    dec.sort_by(f64::total_cmp);
    slack.sort_by(f64::total_cmp);
    slack_dec.sort_by(f64::total_cmp);
    let pick = |v: &[f64], p: f64| v.get(((v.len().saturating_sub(1)) as f64 * p).round() as usize).copied().unwrap_or(0.0);
    let (w, h) = started.unwrap_or_default();
    println!("--- stream-test: {:.1} s of comp {} from {:.2} s at {w}×{h} ---", frames_total as f64 / fps, comp.0, start.seconds());
    println!("warm-up (start request → first chunk decoded): {:.0} ms", ps * 1000.0);
    println!(
        "chunks: {} sent ({} distinct times); arrived after their deadline (after the warm-up): {late}; slack on arrival p10 {:.0} ms / p50 {:.0} ms / min {:.0} ms",
        arrivals.len(),
        firsts.len(),
        pick(&slack, 0.1) * 1000.0,
        pick(&slack, 0.5) * 1000.0,
        slack.first().copied().unwrap_or(0.0) * 1000.0
    );
    println!(
        "decoded after their deadline: {late_dec}; slack once decoded p10 {:.0} ms / min {:.0} ms",
        pick(&slack_dec, 0.1) * 1000.0,
        slack_dec.first().copied().unwrap_or(0.0) * 1000.0
    );
    println!("resolution of the first copy per chunk: {by_scale:?} (1 = full); upgrades sent: {}", arrivals.len().saturating_sub(firsts.len()));
    println!(
        "stalls: {stalls}, {:.0} ms in all; frames shown {shown}/{frames_total}; end {}",
        stalled * 1000.0,
        if ended { "received" } else { "not received" }
    );
    println!(
        "bitrate: {:.2} Mbit/s over {wall:.1} s ({} KB); decode p50 {:.1} ms/frame (4 threads)",
        bytes as f64 * 8.0 / wall / 1e6,
        bytes / 1024,
        pick(&dec, 0.5)
    );
    if let Some(e) = edited_at.filter(|e| *e >= 0.0) {
        println!(
            "edit: stale notice {} ms after the edit; first chunk of the new revision {} ms after it",
            stale_at.map_or("–".into(), |t| format!("{:.0}", (t - e) * 1000.0)),
            first_new_chunk.map_or("–".into(), |t| format!("{:.0}", (t - e) * 1000.0))
        );
    }
    if late > 0 && worst > 0.0 {
        println!("worst lateness: {:.0} ms", worst * 1000.0);
    }
    Ok(())
}

/// A visible edit at comp time `at`: the opacity of the top layer showing then, to 50 %.
fn edit(s: &mut effectcraft_engine::Session, comp: ItemId, at: Tick) -> Result<String, String> {
    let c = s.project.comp(comp).ok_or("no comp")?;
    let l = c.layers.iter().find(|l| l.in_point <= at && at < l.out_point && l.switches.video).ok_or("no layer showing")?;
    let (id, name) = (l.id, l.name.clone());
    s.execute("prop.set", json!({"comp": comp.0, "layer": id.0, "path": "transform/opacity", "value": 50.0})).map_err(|e| e.to_string())?;
    Ok(format!("opacity of layer {} `{name}` to 50 %", id.0))
}
