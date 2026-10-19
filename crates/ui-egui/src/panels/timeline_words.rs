//! Words drawn on the voice layer's bar in the Timeline (SX3): a chip per word where it fits,
//! zoomed out a dot per word (on its stressed syllable, brighter), and a tooltip with the word, its
//! frames and its stress frame on hover. The words come from the comp's speech map and are drawn
//! only on its voice layer, inside the layer's in/out (trimmed words are hidden, not lost).
//!
//! The Timeline calls [`paint`] once per layer bar (one hook in `timeline.rs`). It only paints and
//! reads the hover position: clicks and drags stay with the bar under it.
//!
//! Automation ids: `timeline.layer.<id>.word.<index>` for each word drawn (at most
//! [`MAX_AUTOMATION`] per bar).

use effectcraft_engine::project::{Comp, Layer};
use egui::{Align2, Color32, Pos2, Rect, Stroke, pos2, vec2};

use super::timeline::TMap;
use crate::EffectcraftApp;
use crate::theme::Tokens;

/// Most words a bar registers for automation.
const MAX_AUTOMATION: usize = 400;
/// Most words a bar draws (the rest are off screen anyway at any useful zoom).
const MAX_DRAWN: usize = 4_000;

const CHIP: Color32 = Color32::from_rgba_premultiplied(0x0c, 0x22, 0x17, 0xd0);
const CHIP_STRESSED: Color32 = Color32::from_rgba_premultiplied(0x10, 0x3a, 0x26, 0xe0);
const TEXT: Color32 = Color32::from_rgb(0xb8, 0xd8, 0xc6);
const TEXT_STRESSED: Color32 = Color32::from_rgb(0xe6, 0xff, 0xf0);
const DOT: Color32 = Color32::from_rgb(0x5f, 0x9c, 0x7a);
const DOT_STRESSED: Color32 = Color32::from_rgb(0x8f, 0xff, 0xc0);

/// Where a peak lands on a stressed syllable: a frame before its vowel, never before its onset
/// (comp seconds), as `speech.get {stressed}` and `layer.words` report it.
fn accent(s: &effectcraft_engine::project::speech::Syllable, fps: f64) -> f64 {
    let (t, v) = (s.t.seconds(), s.vowel.seconds());
    if fps > 0.0 { (v - 1.0 / fps).max(t) } else { t }
}

/// Paint the words of `layer` on its bar `bar` (when it is the comp's voice layer).
pub(crate) fn paint(app: &mut EffectcraftApp, ui: &egui::Ui, gp: &egui::Painter, bar: Rect, tm: TMap, comp: &Comp, layer: &Layer) {
    let Some(map) = comp.speech.as_ref() else { return };
    if map.layer != Some(layer.id) || map.words.is_empty() || tm.pps <= 0.0 {
        return;
    }
    let fps = comp.frame_rate.as_f64();
    let frame = |s: f64| (s * fps).round() as i64;
    // Only the words on screen and inside the layer's in/out.
    let clip = gp.clip_rect().intersect(bar);
    if clip.width() <= 0.0 {
        return;
    }
    let (t0, t1) = (tm.t(clip.min.x).max(layer.in_point.seconds()), tm.t(clip.max.x).min(layer.out_point.seconds()));
    let first = map.words.partition_point(|w| w.end.seconds() < t0);
    let hover = ui.ctx().pointer_hover_pos().filter(|p| bar.expand(3.0).contains(*p) && !ui.input(|i| i.pointer.any_down()));
    let mut hovered: Option<(usize, String)> = None;
    let mid = bar.center().y;
    let h = (bar.height() - 6.0).clamp(8.0, 16.0);
    let mut registered = 0usize;
    for (i, w) in map.words.iter().enumerate().skip(first).take(MAX_DRAWN) {
        let (a, b) = (w.start.seconds(), w.end.seconds());
        if a > t1 {
            break;
        }
        if b < t0 {
            continue;
        }
        let stress = w.stressed().map(|(_, s)| accent(s, fps));
        let (xa, xb) = (tm.x(a.max(layer.in_point.seconds())), tm.x(b.min(layer.out_point.seconds())));
        let label_w = w.word.chars().count() as f32 * 6.0 + 8.0;
        let hit = if xb - xa >= label_w {
            let r = Rect::from_min_max(pos2(xa + 0.5, mid - h / 2.0), pos2((xb - 0.5).max(xa + 1.0), mid + h / 2.0));
            gp.rect_filled(r, 3.0, if stress.is_some() { CHIP_STRESSED } else { CHIP });
            gp.with_clip_rect(r.intersect(clip)).text(
                pos2(r.min.x + 4.0, mid),
                Align2::LEFT_CENTER,
                &w.word,
                Tokens::ui(10.5),
                if stress.is_some() { TEXT_STRESSED } else { TEXT },
            );
            if let Some(s) = stress {
                // A tick where the peak should land.
                let x = tm.x(s);
                gp.line_segment([pos2(x, r.max.y - 3.0), pos2(x, r.max.y)], Stroke::new(1.5, DOT_STRESSED));
            }
            r
        } else {
            let x = tm.x(stress.unwrap_or((a + b) / 2.0));
            let (rad, col) = if stress.is_some() { (2.5, DOT_STRESSED) } else { (1.6, DOT) };
            gp.circle_filled(pos2(x, mid), rad, col);
            Rect::from_center_size(pos2(x, mid), vec2(8.0, h))
        };
        if registered < MAX_AUTOMATION && hit.intersects(clip) {
            app.auto.add(&format!("timeline.layer.{}.word.{i}", layer.id.0), hit, &w.word);
            registered += 1;
        }
        if hovered.is_none() && hover.is_some_and(|p: Pos2| hit.contains(p)) {
            let span = format!("f{}–f{}", frame(a), frame(b));
            let tip = match stress {
                Some(s) => format!("{}   {span} · stress f{}", w.word, frame(s)),
                None => format!("{}   {span}", w.word),
            };
            hovered = Some((i, tip));
        }
    }
    if let Some((i, tip)) = hovered {
        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::new(("tl-word-tip", layer.id.0, i)), egui::PopupAnchor::Pointer)
            .show(|ui| ui.label(tip));
    }
}
