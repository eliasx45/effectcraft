//! The playback stream's scheduler with a stand-in renderer: chunks in play order covering the
//! range, an edit that renders again from where it changes, a seek that starts a new grid.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use effectcraft_engine::project::build::layer;
use effectcraft_engine::project::{Comp, ItemId, ItemKind, LayerSource, Project};
use effectcraft_engine::remote::FrameReply;
use effectcraft_engine::remote_stream::{StreamCodec, StreamMsg, StreamReply};
use effectcraft_engine::remote_wire::Down;
use effectcraft_engine::render::{LayerCache, RenderOpts};
use effectcraft_engine::time::{FrameRate, Tick};
use effectcraft_media::preview_stream::decode_chunk;

use super::*;
use crate::frame_server::{FrameRender, RenderJob, Rendered};

const RATE: FrameRate = FrameRate { num: 30, den: 1 };

/// Frames of the comp's size at the asked scale, grey with red = the first layer's out-point in
/// seconds × 20 (so an edit shows in the pixels).
struct Fake;

impl FrameRender for Fake {
    fn render(&self, job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        let c = job.project.comp(job.comp).ok_or("no comp")?;
        let (w, h) = (((f64::from(c.width) * job.opts.scale).round() as u32).max(2), ((f64::from(c.height) * job.opts.scale).round() as u32).max(2));
        let red = c.layers.iter().next().map_or(0.0, |l| l.out_point.seconds() * 20.0).clamp(0.0, 255.0) as u8;
        let bytes = [red, 128, 128, 255].repeat((w * h) as usize);
        Ok((Rendered::Rgba8 { width: w, height: h, bytes }, false))
    }
}

fn project() -> (Arc<Project>, ItemId) {
    let mut p = Project::default();
    let mut main = Comp::new(64, 36, RATE, Tick::from_seconds_f64(10.0));
    let l = layer(&mut p, &main, "n", LayerSource::Null, (10, 10), None);
    main.layers.push(l);
    let mut late = layer(&mut p, &main, "late", LayerSource::Null, (10, 10), None);
    late.in_point = Tick::from_seconds_f64(1.0);
    main.layers.push(late);
    let id = p.add_item("Main", effectcraft_engine::color::Label::None, None, ItemKind::Comp(Arc::new(main)));
    (Arc::new(p), id)
}

fn streams() -> (Streams, Receiver<(Down, Vec<u8>)>) {
    let (tx, rx) = channel();
    let host = Arc::new(StreamHost {
        render: Arc::new(Fake),
        cache: Arc::new(LayerCache::new(16 << 20)),
        frames: Arc::new(crate::frame_cache::FrameCache::new(64 << 20, None)),
        log: Arc::new(|_: &str| {}),
        render_threads: 3,
    });
    (Streams::new(host, tx, 1), rx)
}

fn start(comp: ItemId, from: f64, to: f64, revision: u64, adaptive: bool) -> StreamMsg {
    StreamMsg::Start {
        stream: 7,
        revision,
        comp,
        from: Tick::from_seconds_f64(from),
        to: Some(Tick::from_seconds_f64(to)),
        loop_from: None,
        looping: false,
        opts: Box::new(RenderOpts::default()),
        codec: StreamCodec::H264,
        crf: 20.0,
        chunk_frames: 15,
        ahead_secs: 10.0,
        adaptive,
        max_size: None,
        cache: None,
    }
}

/// Stream replies until `done` says stop (or 20 s pass).
fn collect(rx: &Receiver<(Down, Vec<u8>)>, mut done: impl FnMut(&StreamReply) -> bool) -> Vec<(StreamReply, Vec<u8>)> {
    let end = Instant::now() + Duration::from_secs(20);
    let mut out = vec![];
    while Instant::now() < end {
        if let Ok((Down::Reply { reply: FrameReply::Stream(r), .. }, payload)) = rx.recv_timeout(Duration::from_millis(50)) {
            let stop = done(&r);
            out.push((r, payload));
            if stop {
                return out;
            }
        }
    }
    panic!("timed out; got {:?}", out.iter().map(|o| &o.0).collect::<Vec<_>>());
}

fn chunks(v: &[(StreamReply, Vec<u8>)]) -> Vec<(i64, u32, u64, Vec<u8>)> {
    v.iter()
        .filter_map(|(r, p)| match r {
            StreamReply::Chunk { first, frames, revision, .. } => Some((*first, *frames, *revision, p.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_stream_sends_its_range_in_chunks_then_renders_again_after_an_edit_and_a_seek() {
    let (p, comp) = project();
    let (mut s, rx) = streams();
    s.synced(Some((1, p.clone())));
    // From 0.4 s: the grid starts there (frame 12), not at frame 0.
    s.message(start(comp, 0.4, 2.0, 1, false));
    let got = collect(&rx, |r| matches!(r, StreamReply::End { .. }));
    assert!(matches!(got.first(), Some((StreamReply::Started { width: 64, height: 36, chunk_frames: 15, .. }, _))), "{:?}", got.first());
    let mut cs = chunks(&got);
    cs.sort_by_key(|c| c.0);
    let spans: Vec<(i64, u32)> = cs.iter().map(|c| (c.0, c.1)).collect();
    assert_eq!(spans, vec![(12, 15), (27, 15), (42, 15), (57, 3)], "frames 12..60 in chunks on the grid from 12");
    for (first, n, rev, bytes) in &cs {
        let frames = decode_chunk(bytes, 1).unwrap();
        assert_eq!(frames.len(), *n as usize, "chunk at {first}");
        assert_eq!((frames[0].width, frames[0].height, *rev), (64, 36, 1), "full size (not adaptive), revision 1");
        assert!((frames[0].rgba[0] as i32 - 200).abs() < 12, "red {}", frames[0].rgba[0]);
    }

    // An edit of the layer that starts at 1 s: frames from 30 on again, the earlier ones stay.
    let mut q = (*p).clone();
    q.comp_mut(comp).unwrap().layers.get_mut(1).unwrap().out_point = Tick::from_seconds_f64(8.0);
    let mut first_layer_edit = q.clone();
    s.synced(Some((2, Arc::new(q))));
    let got = collect(&rx, |r| matches!(r, StreamReply::End { .. }));
    assert!(matches!(&got[0].0, StreamReply::Stale { revision: 2, from, .. } if *from == Tick::from_seconds_f64(1.0)), "{:?}", got[0].0);
    let mut again: Vec<(i64, u32)> = chunks(&got).iter().map(|c| (c.0, c.1)).collect();
    again.sort();
    assert_eq!(again, vec![(27, 15), (42, 15), (57, 3)], "the chunk holding frame 30 and those after it");

    // The first layer changes from its in-point (0): everything, and the pixels show it.
    first_layer_edit.comp_mut(comp).unwrap().layers.get_mut(0).unwrap().out_point = Tick::from_seconds_f64(5.0);
    s.synced(Some((3, Arc::new(first_layer_edit))));
    let got = collect(&rx, |r| matches!(r, StreamReply::End { .. }));
    let cs = chunks(&got);
    assert_eq!(cs.len(), 4);
    for (_, _, rev, bytes) in &cs {
        let frames = decode_chunk(bytes, 1).unwrap();
        assert_eq!(*rev, 3);
        assert!((frames[0].rgba[0] as i32 - 100).abs() < 12, "red {} after the edit", frames[0].rgba[0]);
    }

    // A seek: a new grid from there.
    s.message(StreamMsg::Seek { stream: 7, time: Tick::from_seconds_f64(1.5) });
    let got = collect(&rx, |r| matches!(r, StreamReply::End { .. }));
    let spans: Vec<(i64, u32)> = chunks(&got).iter().map(|c| (c.0, c.1)).collect();
    assert_eq!(spans, vec![(45, 15)]);
    s.message(StreamMsg::Stop { stream: 7 });
    s.close();
}

#[test]
fn a_bad_start_fails_the_stream_and_nothing_else() {
    let (p, comp) = project();
    let (mut s, rx) = streams();
    // No project yet.
    s.message(start(comp, 0.0, 1.0, 1, true));
    s.synced(Some((1, p)));
    // Another revision, another comp.
    s.message(start(comp, 0.0, 1.0, 9, true));
    s.message(start(ItemId(999_999), 0.0, 1.0, 1, true));
    // Messages for a stream that doesn't exist are ignored.
    s.message(StreamMsg::Playhead { stream: 3, time: Tick(0), playing: true, received: None });
    s.message(StreamMsg::Seek { stream: 3, time: Tick(0) });
    let mut fails = 0;
    while let Ok((Down::Reply { reply: FrameReply::Stream(r), .. }, _)) = rx.recv_timeout(Duration::from_millis(300)) {
        assert!(matches!(r, StreamReply::Failed { .. }), "{r:?}");
        fails += 1;
    }
    assert_eq!(fails, 3);
}

#[test]
fn an_adaptive_stream_warms_up_at_quarter_then_half_size_then_sends_full_size_before_playback() {
    let (p, comp) = project();
    let (mut s, rx) = streams();
    s.synced(Some((1, p)));
    s.message(start(comp, 0.0, 3.0, 1, true));
    let got = collect(&rx, |r| matches!(r, StreamReply::End { .. }));
    let mut sizes: Vec<(i64, u32, f32)> = got
        .iter()
        .filter_map(|(r, _)| match r {
            StreamReply::Chunk { first, width, scale, .. } => Some((*first, *width, *scale)),
            _ => None,
        })
        .collect();
    sizes.sort_by_key(|c| c.0);
    // The first chunk at quarter size, the rest of the first burst at half; then all of them
    // again at full size, once the window is full (nothing plays yet).
    let warm: Vec<(i64, u32)> = sizes.iter().filter(|c| c.2 < 1.0).map(|c| (c.0, c.1)).collect();
    assert_eq!(warm, vec![(0, 14), (15, 32), (30, 32), (45, 32), (60, 32), (75, 32)], "quarter (16×9 fits as 14×8: even sizes), then half: {sizes:?}");
    let full: std::collections::BTreeSet<i64> = sizes.iter().filter(|c| c.2 == 1.0).map(|c| c.0).collect();
    assert_eq!(full.into_iter().collect::<Vec<_>>(), vec![0, 15, 30, 45, 60, 75], "{sizes:?}");
}

/// Every frame fails (as the asset guard does while footage is missing).
struct Broken;

impl FrameRender for Broken {
    fn render(&self, _job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        Err("footage missing".into())
    }
}

#[test]
fn a_frame_that_cannot_render_ends_the_stream_once() {
    let (p, comp) = project();
    let (tx, rx) = channel();
    let host = Arc::new(StreamHost {
        render: Arc::new(Broken),
        cache: Arc::new(LayerCache::new(16 << 20)),
        frames: Arc::new(crate::frame_cache::FrameCache::new(64 << 20, None)),
        log: Arc::new(|_: &str| {}),
        render_threads: 3,
    });
    let mut s = Streams::new(host, tx, 1);
    s.synced(Some((1, p)));
    s.message(start(comp, 0.0, 2.0, 1, true));
    let got = collect(&rx, |r| matches!(r, StreamReply::Failed { .. }));
    assert!(matches!(&got.last().unwrap().0, StreamReply::Failed { error, .. } if error.contains("footage missing")));
    // Nothing more: no retry storm, no second failure.
    std::thread::sleep(Duration::from_millis(300));
    let more: Vec<_> = rx
        .try_iter()
        .filter(|(d, _)| matches!(d, Down::Reply { reply: FrameReply::Stream(StreamReply::Failed { .. } | StreamReply::Chunk { .. }), .. }))
        .collect();
    assert!(more.is_empty(), "{} more replies", more.len());
}

/// A renderer bug: every frame panics.
struct Panics;

impl FrameRender for Panics {
    fn render(&self, _job: &RenderJob<'_>) -> Result<(Rendered, bool), String> {
        panic!("synthetic renderer bug")
    }
}

/// Edit audit, 18 Oct: a panicking render ended its worker thread, so a stream whose workers all
/// panicked kept its chunks in flight forever (17 Oct: chunks 17–22 at 14.5 s) and the app waited.
/// Now the stream fails once with the reason, as for any frame that can't render.
#[test]
fn a_renderer_panic_fails_the_stream_instead_of_leaving_chunks_in_flight() {
    let (p, comp) = project();
    let (tx, rx) = channel();
    let host = Arc::new(StreamHost {
        render: Arc::new(Panics),
        cache: Arc::new(LayerCache::new(16 << 20)),
        frames: Arc::new(crate::frame_cache::FrameCache::new(64 << 20, None)),
        log: Arc::new(|_: &str| {}),
        render_threads: 3,
    });
    let mut s = Streams::new(host, tx, 1);
    s.synced(Some((1, p)));
    s.message(start(comp, 0.0, 2.0, 1, true));
    let got = collect(&rx, |r| matches!(r, StreamReply::Failed { .. }));
    assert!(
        matches!(&got.last().unwrap().0, StreamReply::Failed { error, .. } if error.contains("panicked") && error.contains("synthetic renderer bug")),
        "{:?}",
        got.last()
    );
}

/// Edit audit, 18 Oct: chunks whose frames the frame cache holds (CR8) go out at full size from
/// the first one: no quarter/half warm-up copy and its upgrade after (a replay or a jump into
/// frames the server has).
#[test]
fn cached_frames_stream_at_full_size_from_the_first_chunk() {
    let (p, comp) = project();
    let (mut s, rx) = streams();
    s.synced(Some((1, p)));
    let series = effectcraft_engine::remote::CacheSeries::new(0xabc, 1000, 0);
    // The server holds frames 0–29 (the first two chunks) at full size.
    let frames = s.host.frames.clone();
    for f in 0..30 {
        frames.put(series.key(f).unwrap(), 64, 36, Arc::new([10u8, 128, 128, 255].repeat(64 * 36)));
    }
    let mut msg = start(comp, 0.0, 3.0, 1, true);
    if let StreamMsg::Start { cache, .. } = &mut msg {
        *cache = Some(series);
    }
    s.message(msg);
    let got = collect(&rx, |r| matches!(r, StreamReply::End { .. }));
    let first_copies: std::collections::BTreeMap<i64, f32> = got.iter().fold(Default::default(), |mut m, (r, _)| {
        if let StreamReply::Chunk { first, scale, .. } = r {
            m.entry(*first).or_insert(*scale);
        }
        m
    });
    assert_eq!(first_copies.get(&0), Some(&1.0), "{first_copies:?}");
    assert_eq!(first_copies.get(&15), Some(&1.0), "{first_copies:?}");
    assert!(first_copies.get(&30).is_some_and(|s| *s < 1.0), "uncached frames still warm up below full size: {first_copies:?}");
}
