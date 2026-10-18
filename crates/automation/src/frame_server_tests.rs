use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use effectcraft_engine::Session;
use effectcraft_engine::remote::{FrameMsg, FrameReply, Mirror, rgba8_premultiplied};
use effectcraft_engine::remote_encode::{Codec, Pixels, Want, decode};
use effectcraft_engine::remote_wire::{Backoff, Down, Event, FrameClient, Up};
use effectcraft_engine::render::{LayerCache, RenderOpts, Renderer};
use effectcraft_engine::time::Tick;
use serde_json::json;

use super::*;
use crate::bridge::ControlTransport;
use crate::frame_link::RemoteControl;

const TOKEN: &str = "TESTTOKEN123";
const WAIT: Duration = Duration::from_secs(20);

fn quick() -> Backoff {
    Backoff { first: Duration::from_millis(20), max: Duration::from_millis(200) }
}

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

fn start(s: &Session, threads: usize, control: Option<ControlFn>) -> FrameHandle {
    spawn(FrameConfig {
        port: 0,
        token: TOKEN.into(),
        threads,
        render: Arc::new(CpuRender { footage: s.footage.clone(), expr: s.expr.clone() }),
        cache: Arc::new(LayerCache::new(256 << 20)),
        frames: Arc::new(crate::frame_cache::FrameCache::new(256 << 20, None)),
        preload: None,
        control,
        log: Arc::new(|_| {}),
    })
    .unwrap()
}

/// The next event that matches, skipping others.
fn wait_for<T>(c: &FrameClient, mut f: impl FnMut(Event) -> Option<T>) -> T {
    let end = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < end {
        if let Some(ev) = c.recv(Duration::from_millis(50))
            && let Some(t) = f(ev)
        {
            return t;
        }
    }
    panic!("timed out");
}

fn connected(c: &FrameClient) -> Down {
    wait_for(c, |e| match e {
        Event::Connected { welcome } => Some(welcome),
        _ => None,
    })
}

fn reply(c: &FrameClient) -> (FrameReply, Option<Pixels>, Vec<u8>) {
    wait_for(c, |e| match e {
        Event::Down(Down::Reply { reply, pixels }, p) => Some((reply, pixels, p)),
        _ => None,
    })
}

#[test]
fn a_megabyte_goes_there_and_back() {
    let s = demo();
    let h = start(&s, 2, None);
    let c = FrameClient::connect(&format!("127.0.0.1:{}", h.port), TOKEN, "test", quick());
    let Down::Welcome { version, threads, revision, .. } = connected(&c) else { panic!() };
    assert_eq!((version, threads, revision), (1, 2, None));
    let big: Vec<u8> = (0..(1u32 << 20)).map(|i| (i * 31 % 251) as u8).collect();
    let t0 = std::time::Instant::now();
    assert!(c.send(Up::Echo { id: 9 }, big.clone()));
    let back = wait_for(&c, |e| match e {
        Event::Down(Down::Echo { id: 9 }, p) => Some(p),
        _ => None,
    });
    assert_eq!(back, big);
    assert!(t0.elapsed() < WAIT);
    h.stop();
}

#[test]
fn a_wrong_token_is_refused_and_locks_out() {
    let s = demo();
    let h = start(&s, 1, None);
    let addr = format!("127.0.0.1:{}", h.port);
    let c = FrameClient::connect(&addr, "WRONGWRONG", "test", quick());
    let error = wait_for(&c, |e| match e {
        Event::Refused { error } => Some(error),
        Event::Connected { .. } => panic!("a wrong token connected"),
        _ => None,
    });
    assert!(error.contains("wrong token"), "{error}");
    // No hello at all: refused too (an HTTP request, a JSON line).
    let mut raw = TcpStream::connect(&addr).unwrap();
    std::io::Write::write_all(&mut raw, b"{\"id\":1,\"method\":\"app.revision\"}\n").unwrap();
    let mut buf = vec![];
    let _ = std::io::Read::read_to_end(&mut raw, &mut buf);
    assert!(String::from_utf8_lossy(&buf).contains("hello"), "{}", String::from_utf8_lossy(&buf));
    // Four more wrong tokens lock everyone out, the right token included, for a while.
    for _ in 0..4 {
        let c = FrameClient::connect(&addr, "WRONGWRONG", "test", quick());
        wait_for(&c, |e| matches!(e, Event::Refused { .. }).then_some(()));
    }
    let c = FrameClient::connect(&addr, TOKEN, "test", quick());
    let error = wait_for(&c, |e| match e {
        Event::Refused { error } => Some(error),
        _ => None,
    });
    assert!(error.contains("too many"), "{error}");
    h.stop();
}

/// A stand-in for the SSH tunnel: forwards `port` to the server until killed.
struct Tunnel {
    stop: Arc<std::sync::atomic::AtomicBool>,
    conns: Arc<Mutex<Vec<TcpStream>>>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Tunnel {
    fn open(port: u16, to: u16) -> Tunnel {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let conns: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (held, stopped) = (Arc::clone(&conns), Arc::clone(&stop));
        let accept = std::thread::spawn(move || {
            for a in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(a) = a else { return };
                let Ok(b) = TcpStream::connect(("127.0.0.1", to)) else { return };
                held.lock().unwrap().extend([a.try_clone().unwrap(), b.try_clone().unwrap()]);
                let pipe = |mut from: TcpStream, mut to: TcpStream| {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut to);
                        let _ = to.shutdown(Shutdown::Both);
                    });
                };
                pipe(a.try_clone().unwrap(), b.try_clone().unwrap());
                pipe(b, a);
            }
        });
        Tunnel { stop, conns, accept: Some(accept) }
    }

    /// Kill it: its connections drop and nothing listens on its port any more.
    fn kill(mut self, port: u16) {
        self.stop.store(true, Ordering::SeqCst);
        for c in self.conns.lock().unwrap().drain(..) {
            let _ = c.shutdown(Shutdown::Both);
        }
        let _ = TcpStream::connect(("127.0.0.1", port));
        if let Some(t) = self.accept.take() {
            t.join().unwrap();
        }
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn the_client_reconnects_after_the_tunnel_dies() {
    let s = demo();
    let h = start(&s, 1, None);
    let port = free_port();
    let tunnel = Tunnel::open(port, h.port);
    let c = FrameClient::connect(&format!("127.0.0.1:{port}"), TOKEN, "test", quick());
    connected(&c);
    assert!(c.send(Up::Echo { id: 1 }, vec![1]));
    wait_for(&c, |e| matches!(e, Event::Down(Down::Echo { id: 1 }, _)).then_some(()));
    tunnel.kill(port);
    wait_for(&c, |e| matches!(e, Event::Disconnected { .. }).then_some(()));
    assert!(!c.send(Up::Echo { id: 2 }, vec![2]), "nothing is queued while the tunnel is down");
    std::thread::sleep(Duration::from_millis(300));
    // The tunnel comes back on the same port: the client finds it on its own.
    let _tunnel = Tunnel::open(port, h.port);
    connected(&c);
    assert!(c.send(Up::Echo { id: 3 }, vec![3]));
    let p = wait_for(&c, |e| match e {
        Event::Down(Down::Echo { id: 3 }, p) => Some(p),
        Event::Down(Down::Echo { id: 2 }, _) => panic!("a message from before the drop was sent"),
        _ => None,
    });
    assert_eq!(p, vec![3]);
    // A dropped connection (not the tunnel) reconnects too.
    c.drop_connection();
    connected(&c);
    h.stop();
}

fn render_msg(id: u64, revision: u64, comp: effectcraft_engine::project::ItemId, t: Tick, opts: RenderOpts) -> FrameMsg {
    FrameMsg::Render { id, revision, comp, time: t, opts: Box::new(opts), disk: None, layers: false, prefetch: vec![] }
}

#[test]
fn frames_render_stale_ones_fail_and_refines_are_exact() {
    let mut s = demo();
    let comp = s.active_comp_id().unwrap();
    let h = start(&s, 2, None);
    let c = FrameClient::connect(&format!("127.0.0.1:{}", h.port), TOKEN, "test", quick());
    connected(&c);
    let mut mirror = Mirror::default();
    let sync = |c: &FrameClient, m: &mut Mirror, s: &Session| {
        let msg = m.sync(s.revision, &s.project).unwrap();
        assert!(c.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]));
    };
    sync(&c, &mut mirror, &s);
    let opts = RenderOpts { scale: 0.25, ..Default::default() };
    let t = Tick::from_seconds_f64(1.0);
    let want = Want { codec: Codec::Jpeg, quality: 90, max_size: None, refine: true };
    assert!(c.send(Up::Msg { msg: render_msg(1, s.revision, comp, t, opts), want, urgent: true }, vec![]));
    let local = {
        let mut r = Renderer::new(&s.project, s.footage.as_ref(), opts);
        r.expr = s.expr.as_deref();
        rgba8_premultiplied(&r.comp_frame_cpu(comp, t))
    };
    // The JPEG first, then the lossless refine of the same request.
    let (r, px, payload) = reply(&c);
    let FrameReply::Frame { id: 1, width, height, gpu: false, .. } = r else { panic!("{r:?}") };
    let px = px.unwrap();
    assert_eq!((px.codec, px.refine, px.width, px.height), (Codec::Jpeg, false, width, height));
    let jpeg = decode(&px, &payload).unwrap();
    assert_eq!(jpeg.len(), local.len());
    let mean = jpeg.iter().zip(&local).map(|(a, b)| f64::from(a.abs_diff(*b))).sum::<f64>() / local.len() as f64;
    assert!(mean < 4.0, "JPEG mean error {mean}");
    let (r, px, payload) = reply(&c);
    assert!(matches!(r, FrameReply::Frame { id: 1, .. }));
    let px = px.unwrap();
    assert_eq!((px.codec, px.refine), (Codec::Qoi, true));
    assert_eq!(decode(&px, &payload).unwrap(), local, "the refine is exact");
    // Scaled to the viewer's size.
    let small = Want { codec: Codec::Raw, quality: 85, max_size: Some([width / 2, height / 2]), refine: false };
    assert!(c.send(Up::Msg { msg: render_msg(2, s.revision, comp, t, opts), want: small, urgent: true }, vec![]));
    let (r, px, _) = reply(&c);
    assert!(matches!(r, FrameReply::Frame { id: 2, .. }), "{r:?}");
    assert_eq!(px.unwrap().width, width / 2);
    // An edit: the old revision is stale, the new one renders.
    let old = s.revision;
    s.execute("layer.newSolid", json!({"name": "Edit", "color": "#ff0000"})).unwrap();
    sync(&c, &mut mirror, &s);
    assert!(c.send(Up::Msg { msg: render_msg(3, old, comp, t, opts), want, urgent: true }, vec![]));
    let (r, ..) = reply(&c);
    assert!(matches!(r, FrameReply::Failed { id: 3, stale: true, .. }), "{r:?}");
    assert!(c.send(Up::Msg { msg: render_msg(4, s.revision, comp, t, opts), want: Want { refine: false, ..want }, urgent: true }, vec![]));
    let (r, ..) = reply(&c);
    assert!(matches!(r, FrameReply::Frame { id: 4, .. }), "{r:?}");
    // A patch that does not apply asks for the whole project.
    assert!(c.send(
        Up::Msg { msg: FrameMsg::Patch(effectcraft_engine::remote_sync::Patch { from_rev: 999, to_rev: 1000, ops: vec![] }), want, urgent: false },
        vec![]
    ));
    let (r, ..) = reply(&c);
    assert!(matches!(r, FrameReply::Resync { .. }), "{r:?}");
    h.stop();
}

fn job(conn: u64, id: u64, urgent: bool, s: &Session, out: &Out) -> Job {
    Job {
        conn,
        id,
        revision: 0,
        urgent,
        seq: 0,
        project: s.project.clone(),
        comp: effectcraft_engine::project::ItemId(1),
        time: Tick::ZERO,
        opts: RenderOpts::default(),
        want: Want::default(),
        key: None,
        out: out.clone(),
        current: Arc::new(AtomicU64::new(0)),
        queued: Instant::now(),
    }
}

#[test]
fn urgent_frames_go_first_newest_first_and_cancel_by_id() {
    let s = Session::default();
    let (out, _rx) = channel();
    let q = Sched::default();
    for (id, urgent) in [(1, false), (2, false), (3, true), (4, true), (5, false)] {
        q.push(job(1, id, urgent, &s, &out));
    }
    q.cancel(1, &[5]);
    q.cancel(2, &[1]);
    let mut order = vec![];
    for _ in 0..4 {
        let Some((Work::Render(j), _)) = q.next(&FrameCache::new(0, None)) else { panic!() };
        order.push(j.id);
        assert!(!q.done(j.conn, j.id));
    }
    assert_eq!(order, vec![4, 3, 1, 2]);
    // Cancelling a frame that is rendering drops its reply.
    q.push(job(1, 6, true, &s, &out));
    let Some((Work::Render(j), _)) = q.next(&FrameCache::new(0, None)) else { panic!() };
    q.cancel(1, &[6]);
    assert!(q.done(j.conn, j.id), "cancelled while rendering");
    // A closed connection's work is gone.
    q.push(job(7, 1, true, &s, &out));
    q.close(7);
    assert!(lock(&q.q).jobs.is_empty());
}

#[test]
fn control_requests_go_through_the_frame_channel() {
    let s = demo();
    let control: ControlFn = Arc::new(|method, params| json!({"ok": true, "result": {"method": method, "params": params}}));
    let h = start(&s, 1, Some(control));
    let mut rc = RemoteControl::with_backoff(&format!("127.0.0.1:{}", h.port), TOKEN, quick());
    let r = rc.call("app.revision", json!({"x": 1})).unwrap();
    assert_eq!(r, json!({"method": "app.revision", "params": {"x": 1}}));
    assert_eq!(rc.mode(), "frames");
    // Without a control host: an error reply, not a hang.
    let h2 = start(&s, 1, None);
    let mut rc2 = RemoteControl::with_backoff(&format!("127.0.0.1:{}", h2.port), TOKEN, quick());
    assert!(matches!(rc2.call("app.revision", json!({})), Err(crate::Error::App(_))));
    // A wrong token is an error too.
    let mut bad = RemoteControl::with_backoff(&format!("127.0.0.1:{}", h2.port), "WRONGWRONG", quick());
    let e = bad.call("app.revision", json!({})).unwrap_err().to_string();
    assert!(e.contains("refused"), "{e}");
    h.stop();
    h2.stop();
}

#[test]
fn tokens_come_from_the_flag_the_environment_or_a_private_file() {
    let dir = std::env::temp_dir().join(format!("ec-frame-token-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(frame_token(Some(&dir), Some("explicit-token")).unwrap().0, "explicit-token");
    assert!(frame_token(Some(&dir), Some("short")).is_err());
    if std::env::var("EFFECTCRAFT_FRAME_TOKEN").is_err() {
        let (a, stored) = frame_token(Some(&dir), None).unwrap();
        assert_eq!(stored.as_deref(), Some(dir.join(TOKEN_FILE).as_path()));
        assert_eq!(frame_token(Some(&dir), None).unwrap().0, a, "reused");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// CR8: a precache plan renders into the server's frame cache without sending pixels and
/// reports the cached frames as ranges; a render request naming a cached frame's key is
/// answered from the cache (no render); a stream that plays holds the plan back.
#[test]
fn precache_fills_the_frame_cache_and_cached_frames_skip_the_render() {
    use effectcraft_engine::remote::{CacheSeries, Precache};
    let s = demo();
    let comp = s.active_comp_id().unwrap();
    let frames = Arc::new(FrameCache::new(256 << 20, None));
    let h = spawn(FrameConfig {
        port: 0,
        token: TOKEN.into(),
        threads: 2,
        render: Arc::new(CpuRender { footage: s.footage.clone(), expr: s.expr.clone() }),
        cache: Arc::new(LayerCache::new(64 << 20)),
        frames: frames.clone(),
        preload: None,
        control: None,
        log: Arc::new(|_| {}),
    })
    .unwrap();
    let c = FrameClient::connect(&format!("127.0.0.1:{}", h.port), TOKEN, "test", quick());
    connected(&c);
    let mut mirror = Mirror::default();
    let msg = mirror.sync(s.revision, &s.project).unwrap();
    assert!(c.send(Up::Msg { msg, want: Want::default(), urgent: false }, vec![]));
    let opts = RenderOpts { scale: 0.1, ..Default::default() };
    let series = CacheSeries::new(0x1234_5678_9abc_def0_1234_5678_9abc_def0, 100, 42);
    let plan = Precache { revision: s.revision, comp, series: series.clone(), opts: Box::new(opts), ranges: vec![(4, 7), (0, 1)], prints: None };
    assert!(c.send(Up::Msg { msg: FrameMsg::Precache(plan), want: Want::default(), urgent: false }, vec![]));
    // The first report (nothing cached yet), then the last once all six frames are in.
    let mut last = None;
    while last.is_none() {
        let (r, px, payload) = reply(&c);
        assert!(px.is_none() && payload.is_empty(), "a precache sends no pixels");
        let FrameReply::Cached { done, total, ranges, series: got, frames: n, .. } = r else { panic!("{r:?}") };
        assert_eq!((total, &got), (6, &series));
        if done == total {
            last = Some((ranges, n));
        }
    }
    assert_eq!(last, Some((vec![(0, 1), (4, 7)], 6)));
    for f in [0, 1, 4, 5, 6, 7] {
        assert!(frames.contains(series.key(f).unwrap()), "frame {f} cached");
    }
    assert!(!frames.contains(series.key(2).unwrap()));
    // A viewer request for a cached frame: from the cache, the same pixels as a render.
    let t = s.project.comp(comp).unwrap().frame_rate.tick_of(5);
    let key = format!("{:032x}", series.key(5).unwrap());
    let msg = FrameMsg::Render { id: 9, revision: s.revision, comp, time: t, opts: Box::new(opts), disk: Some(key), layers: false, prefetch: vec![] };
    let raw = Want { codec: Codec::Raw, quality: 85, max_size: None, refine: false };
    assert!(c.send(Up::Msg { msg, want: raw, urgent: true }, vec![]));
    let (r, px, payload) = reply(&c);
    let FrameReply::Frame { id: 9, ms, .. } = r else { panic!("{r:?}") };
    assert_eq!(ms, 0.0, "not rendered");
    let local = {
        let mut r = Renderer::new(&s.project, s.footage.as_ref(), opts);
        r.expr = s.expr.as_deref();
        rgba8_premultiplied(&r.comp_frame_cpu(comp, t))
    };
    assert_eq!(decode(&px.unwrap(), &payload).unwrap(), local);
    assert_eq!(frames.stats().hits, 1);
    h.stop();
}
