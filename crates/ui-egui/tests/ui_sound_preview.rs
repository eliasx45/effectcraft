//! Motion Library hover-to-hear: the Sound card under the pointer plays through the app's audio
//! output at the SFX preview volume, stops when the pointer leaves, and stays silent while the
//! Timeline plays.

use std::sync::{Arc, Mutex};

use effectcraft_engine::Session;
use effectcraft_ui_egui::audio::{AudioDevice, AudioFeed};
use effectcraft_ui_egui::{EffectcraftApp, SoundHover};
use egui_kittest::Harness;
use serde_json::json;

/// An output that keeps the feed it is given and how often it was opened.
#[derive(Clone, Default)]
struct Out(Arc<Mutex<(Option<Arc<AudioFeed>>, usize)>>);

impl AudioDevice for Out {
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn start(&mut self, feed: Arc<AudioFeed>) -> Result<(), String> {
        let mut g = self.0.lock().unwrap();
        g.0 = Some(feed);
        g.1 += 1;
        Ok(())
    }
    fn stop(&mut self) {}
}

/// A 16-bit stereo WAV: a 440 Hz sine at `amp`, `secs` long.
fn sine_wav(path: &std::path::Path, secs: f64, amp: f32) {
    let rate = 44_100u32;
    let n = (rate as f64 * secs) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + n * 4).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), rate.to_le_bytes().to_vec()] {
        b.extend_from_slice(&v);
    }
    b.extend_from_slice(&(rate * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(n * 4).to_le_bytes());
    for i in 0..n {
        let v = ((i as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * amp * 32767.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let t = std::time::Instant::now();
    while !ok() {
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn hovering_a_sound_card_plays_it_at_the_sfx_volume_and_leaving_stops_it() {
    let dir = std::env::temp_dir().join(format!("ec-ui-sound-preview-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("Hit.wav");
    sine_wav(&wav, 0.5, 0.5);
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "A", "width": 64, "height": 36, "frameRate": 30, "duration": 2})).unwrap();
    let out = Out::default();
    let o = out.clone();
    let mut app = EffectcraftApp::new(s);
    app.hooks.audio_device = Some(Box::new(move |_| Some(Box::new(o.clone()) as Box<dyn AudioDevice>)));
    let mut h = Harness::builder().with_size(egui::vec2(800.0, 500.0)).build_eframe(|_| app);
    h.run_steps(2);
    let hover = SoundHover { key: "sfx-1".into(), path: wav.to_string_lossy().to_string(), gain: 0.2 };

    // Over the card: it plays, at 20 %.
    h.state_mut().sound_hover = Some(hover.clone());
    h.step();
    assert_eq!(h.state().sound_preview.as_ref().and_then(|p| p.playing()), Some("sfx-1"));
    let feed = out.0.lock().unwrap().0.clone().expect("the output opened");
    wait_for("the sound", || feed.queued_frames() >= 2000);
    let mut buf = vec![0.0f32; 4000];
    feed.pull(&mut buf);
    let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 0.1).abs() < 0.01, "0.5 × 20 % = 0.1, got {peak}");

    // Still over it (the volume turned up): the same sound goes on, the output isn't reopened.
    h.state_mut().sound_hover = Some(SoundHover { gain: 0.6, ..hover.clone() });
    h.step();
    assert_eq!(h.state().sound_preview.as_ref().map(|p| (p.playing(), p.gain())), Some((Some("sfx-1"), 0.6)));
    assert_eq!(out.0.lock().unwrap().1, 1);

    // The pointer left: it stops (the output stays open a moment for the next card).
    h.step();
    assert!(h.state().sound_preview.as_ref().is_some_and(|p| p.playing().is_none()));
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(feed.queued_frames() <= 240, "only the fade-out is left");

    // While the Timeline plays, hovering is silent.
    h.state_mut().playback.playing = true;
    h.state_mut().sound_hover = Some(hover);
    h.step();
    assert!(h.state().sound_preview.as_ref().is_none_or(|p| p.playing().is_none()));
    h.state_mut().playback.playing = false;
    let _ = std::fs::remove_dir_all(&dir);
}
