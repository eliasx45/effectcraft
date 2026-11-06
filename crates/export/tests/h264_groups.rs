//! E2a: H.264 movies encode in parallel keyframe groups (2 s each) joined into one stream. The
//! joined movie must play as if one encoder wrote it: every frame once, in order, evenly timed,
//! a key frame at each group, the audio intact across the joins.
//!
//! The comp: 64×48 @ 10 fps (groups of 20 frames), a red bar sliding left to right over a blue
//! background, so each frame's bar position says which frame it is, plus a 440 Hz tone.

use std::path::{Path, PathBuf};
use std::process::Command;

use effectcraft_color::Label;
use effectcraft_export::render_queue::{AudioOutput, OutputFormat, OutputModule, RenderSettings, TimeSpan};
use effectcraft_export::{Job, export};
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_media::MediaPool;
use effectcraft_project::{Comp, ItemId, ItemKind, LayerSource, Project, Solid, build};
use effectcraft_time::{FrameRate, Tick};

const W: u32 = 64;
const H: u32 = 48;

fn out_dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-out/export").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("mkdir");
    d
}

/// 16-bit stereo WAV: a 440 Hz sine at amplitude 0.5 on the left, silence on the right.
fn write_tone(path: &Path, secs: f64) {
    let sr = 48_000u32;
    let n = (secs * sr as f64) as usize;
    let mut data = Vec::with_capacity(n * 4);
    for i in 0..n {
        let s = (0.5 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / sr as f64).sin() * 32767.0).round() as i16;
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&0i16.to_le_bytes());
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&sr.to_le_bytes());
    w.extend_from_slice(&(sr * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data.len() as u32).to_le_bytes());
    w.extend_from_slice(&data);
    std::fs::write(path, w).expect("write wav");
}

/// Where the bar's centre is in frame `k` of `n`: x from 6 to 58.
fn bar_x(k: u64, n: u64) -> f32 {
    6.0 + 52.0 * k as f32 / (n - 1) as f32
}

/// A comp of `n` frames with the sliding bar and a tone.
fn project(n: u64, wav: &Path) -> (Project, ItemId) {
    let rate = FrameRate::new(10, 1);
    let mut p = Project::default();
    let mut comp = Comp::new(W, H, rate, rate.tick_of(n as i64));
    comp.background = [0.1, 0.1, 0.4];
    let sid = p.add_item("Bar", Label::Red, None, ItemKind::Solid(Solid { color: [1.0, 0.0, 0.0], width: 4, height: H, pixel_aspect: 1.0 }));
    let mut l = build::layer(&mut p, &comp, "Bar", LayerSource::Solid { item: sid }, (4, H), None);
    let y = H as f64 / 2.0;
    l.props.prop_mut("transform/position").expect("position").keys = vec![
        Keyframe::new(Tick::ZERO, Value::Vec3([bar_x(0, n) as f64, y, 0.0])),
        Keyframe::new(rate.tick_of(n as i64 - 1), Value::Vec3([bar_x(n - 1, n) as f64, y, 0.0])),
    ];
    comp.layers.push(l);
    let f = effectcraft_media::probe(wav).expect("probe wav");
    let fid = p.add_item("tone.wav", Label::SeaFoam, None, ItemKind::Footage(f));
    comp.layers.push(build::layer(&mut p, &comp, "tone", LayerSource::Footage { item: fid }, (0, 0), None));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.into()));
    (p, cid)
}

/// The red bar's centre column in a decoded frame (the mean x of reddish pixels on the middle row).
fn red_x(img: &effectcraft_raster::Image) -> f32 {
    let y = img.height / 2;
    let xs: Vec<f32> = (0..img.width).filter(|&x| img.data[img.idx(x, y)][0] > 0.5).map(|x| x as f32 + 0.5).collect();
    assert!(!xs.is_empty(), "no bar");
    xs.iter().sum::<f32>() / xs.len() as f32
}

fn rms(v: impl Iterator<Item = f32>) -> f32 {
    let (s, n) = v.fold((0.0f64, 0usize), |(s, n), x| (s + (x as f64) * (x as f64), n + 1));
    (s / n.max(1) as f64).sqrt() as f32
}

#[test]
fn keyframe_groups_join_into_a_seamless_movie() {
    let d = out_dir("h264-groups");
    let wav = d.join("tone.wav");
    write_tone(&wav, 6.0);
    let pool = MediaPool::new();
    // 47 frames: groups of 20, 20 and 7; 41 frames: a last group of one frame
    for n in [47u64, 41] {
        let (p, cid) = project(n, &wav);
        let settings = RenderSettings { time_span: TimeSpan::LengthOfComp, ..Default::default() };
        let om = OutputModule { audio: AudioOutput::On, bitrate_kbps: 2000, ..OutputModule::for_format(OutputFormat::H264) };
        let path = d.join(format!("groups-{n}.mp4"));
        let ps = path.to_string_lossy().to_string();
        let job = Job {
            project: &p,
            footage: &pool,
            expr: None,
            accel: None,
            comp: cid,
            settings: &settings,
            output: &om,
            path: &ps,
            sink: None,
            options: Default::default(),
            nested_switches: true,
        };
        let r = export(&job, &mut |_| true).expect("export");
        assert_eq!(r.frames, n);
        assert!(r.audio);

        // Our own decoder: the frame count, and every frame in its place (none twice or missing).
        let f = effectcraft_media::probe(&path).expect("probe");
        assert_eq!(f.frame_rate.frame_at(f.duration - Tick(1)) as u64 + 1, n, "frames from the duration");
        let dec = MediaPool::new();
        for k in 0..n {
            let img = dec.frame_at(&f, f.frame_rate.tick_of(k as i64)).expect("decode");
            let (got, want) = (red_x(&img), bar_x(k, n));
            assert!((got - want).abs() < 1.6, "{n} frames: frame {k} shows the bar at {got}, want {want}");
        }
        // The tone on both sides of the joins (frames 20 and 40 at 2 s and 4 s).
        let a = dec.audio_samples(&f, Tick::from_seconds_f64(1.5), 48_000, 48_000);
        let l = rms(a.iter().step_by(2).copied());
        assert!((l - 0.3536).abs() < 0.03, "{n} frames: left RMS {l} across 1.5..2.5 s");

        // ffprobe (external oracle, skipped without it): the frames, evenly timed, key frames at
        // every group, and the audio as long as the video.
        let Ok(out) = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=pts_time,key_frame", "-of", "csv=p=0"])
            .arg(&path)
            .output()
        else {
            eprintln!("ffprobe not found: skipping the oracle");
            continue;
        };
        assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));
        let rows: Vec<(bool, f64)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                // (side data may add fields after these two)
                let mut f = l.split(',');
                let (k, t) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
                (k.trim() == "1", t.trim().parse::<f64>().unwrap_or_else(|_| panic!("pts in {l:?}")))
            })
            .collect();
        assert_eq!(rows.len() as u64, n, "{n} frames: ffprobe frame count");
        for (k, (key, t)) in rows.iter().enumerate() {
            assert!((t - k as f64 * 0.1).abs() < 1e-3, "{n} frames: frame {k} at {t} s");
            if k % 20 == 0 {
                assert!(key, "{n} frames: frame {k} starts a group but is not a key frame");
            }
        }
        let dur = |stream: &str| -> f64 {
            let o = Command::new("ffprobe")
                .args(["-v", "error", "-select_streams", stream, "-show_entries", "stream=duration", "-of", "csv=p=0"])
                .arg(&path)
                .output()
                .expect("ffprobe");
            String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().expect("duration")
        };
        let (v, a) = (dur("v:0"), dur("a:0"));
        assert!((v - n as f64 / 10.0).abs() < 1e-3, "{n} frames: video lasts {v} s");
        assert!((a - v).abs() < 0.05, "{n} frames: audio {a} s, video {v} s");
    }
}
