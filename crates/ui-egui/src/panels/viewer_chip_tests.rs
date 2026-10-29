//! EMBED17: the hostable resolution chip and the real-time readout a host shows on a playing comp.

use egui::{Event, PointerButton, Pos2, RawInput, Rect, pos2, vec2};

use super::viewer_tools::{paint_rate, resolution_chip};
use crate::Playback;
use crate::state::Resolution;
use crate::theme::{ThemeKind, Tokens};

fn click(at: Pos2) -> Vec<RawInput> {
    let press = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Default::default() };
    vec![
        RawInput { events: vec![Event::PointerMoved(at)], ..Default::default() },
        RawInput { events: vec![press(true)], ..Default::default() },
        RawInput { events: vec![press(false)], ..Default::default() },
        RawInput::default(),
    ]
}

/// The chip shows the bottom bar's label, opens its list on a click and junk scales never panic.
#[test]
fn the_resolution_chip_opens_and_never_panics() {
    let ctx = egui::Context::default();
    let t = Tokens::for_kind(ThemeKind::Dark);
    let rect = Rect::from_min_size(pos2(10.0, 10.0), vec2(84.0, 20.0));
    let id = egui::Id::new("card-res");
    for input in click(rect.center()) {
        let _ = ctx.run_ui(input, |ui| {
            let picked = resolution_chip(ui, rect, Resolution::Half, 0.5, &t, id);
            assert!(picked.is_none(), "a click on the chip opens it, picks nothing");
        });
    }
    assert!(ctx.data(|d| d.get_temp::<bool>(id.with("pop").with("open"))).unwrap_or(false), "the list is open");
    for scale in [f64::NAN, 0.0, -1.0, 1e9] {
        let _ = ctx.run_ui(RawInput::default(), |ui| {
            let _ = resolution_chip(ui, rect, Resolution::Custom(7), scale, &t, id);
        });
    }
}

/// A host's own `Playback` gives the Info panel's readout: real time when the frames it shows keep
/// the rate, not when they fall behind; the chip draws for both.
#[test]
fn a_hosts_playback_says_whether_it_keeps_real_time() {
    let mut pb = Playback { playing: true, fps: 25.0, ..Playback::default() };
    assert!(pb.rate_label().is_none(), "nothing before two frames");
    for i in 0..26 {
        pb.frame_shown(f64::from(i) / 25.0, i64::from(i));
    }
    pb.observe(1.0);
    let (label, rt) = pb.rate_label().unwrap();
    assert!(rt && label.contains("real-time") && !label.contains("not"), "{label}");
    let mut slow = Playback { playing: true, fps: 25.0, ..Playback::default() };
    for i in 0..6 {
        slow.frame_shown(f64::from(i) / 5.0, i64::from(i) * 5);
    }
    slow.observe(1.0);
    let (label, rt) = slow.rate_label().unwrap();
    assert!(!rt && label.contains("not real-time"), "{label}");
    let ctx = egui::Context::default();
    let _ = ctx.run_ui(RawInput::default(), |ui| {
        let r = paint_rate(ui.painter(), pos2(300.0, 10.0), &label, rt);
        assert!(r.max.x <= 300.0 + 1e-3 && r.min.y == 10.0);
        let _ = paint_rate(ui.painter(), pos2(f32::NAN, 0.0), "x", true);
    });
}
