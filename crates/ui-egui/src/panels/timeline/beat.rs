//! Timeline marks for batch 1: the linked-graph tag on a move's first key (AX3, "Fast In ·
//! linked") and the off-beat dot and syllable snap (AX6).

use effectcraft_engine::commands::graph_cmds::linked_name;
use effectcraft_engine::lint;
use effectcraft_engine::project::{Comp, Layer, Property};
use effectcraft_engine::time::Tick;
use egui::{Align2, Color32, Painter, Rect, pos2};

use super::TMap;
use crate::panels::timeline::TimelineHost;
use crate::theme::Tokens;

/// Writes "<graph> · linked" right of the first key of every move of `prop` that carries a
/// Library graph, when there is room before the move's second key.
pub(crate) fn graph_tags(p: &Painter, tm: TMap, layer: &Layer, prop: &Property, cy: f32, x_min: f32, x_max: f32, colour: Color32) {
    if prop.keys.len() < 2 {
        return;
    }
    for i in 0..prop.keys.len() - 1 {
        let (Some(a), Some(b)) = (prop.keys.get(i), prop.keys.get(i + 1)) else { continue };
        let xa = tm.x(layer.comp_time(a.time).seconds()) + 9.0;
        let xb = tm.x(layer.comp_time(b.time).seconds()) - 9.0;
        if xb < x_min || xa > x_max || xb - xa < 40.0 {
            continue;
        }
        let Some(name) = linked_name(&prop.keys, i, prop.spatial) else { continue };
        let text = format!("{name} · linked");
        let g = p.layout_no_wrap(text.clone(), Tokens::ui(10.5), colour);
        if g.size().x > xb - xa {
            continue;
        }
        p.text(pos2(xa, cy), Align2::LEFT_CENTER, text, Tokens::ui(10.5), colour);
    }
}

// ---------------------------------------------------------------- AX6: off-beat dot, syllable snap

/// A layer's first move against the speech map: where its speed peak is and the nearest
/// stressed syllable's accent (comp seconds).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Beat {
    /// The move's first key (comp seconds): where the dot sits.
    pub key: f64,
    pub peak: f64,
    pub word: String,
    pub accent: f64,
    /// Peak − accent, frames.
    pub off: f64,
}

/// Within this many frames a peak is on the beat (green).
pub(crate) const ON_BEAT: f64 = 1.5;
/// A dragged key snaps when its peak comes this close to an accent (frames).
pub(crate) const SNAP_FRAMES: f64 = 2.0;

/// The comp's stressed syllables that can be heard: (word, accent comp seconds), in time order.
/// Lane B (SX1) keeps the words in `comp.speech` in comp time and moves them with their voice
/// layer on every edit; words outside the voice layer's in/out are trimmed away
/// ([`SpeechMap::voice_span`](effectcraft_engine::project::speech::SpeechMap::voice_span)).
pub(crate) fn accents(comp: &Comp) -> Vec<(String, f64)> {
    let Some(map) = comp.speech.as_ref() else { return vec![] };
    let fps = comp.frame_rate.as_f64();
    let (a, b) = map.voice_span(comp);
    map.stressed()
        .into_iter()
        .filter(|(w, _)| w.end > a && w.start < b)
        .map(|(w, s)| (w.word.clone(), lint::accent_time(s, fps, lint::DEFAULT_ACCENT_LEAD)))
        .collect()
}

/// The comp's voice layer (its words move with it, so it never snaps onto them).
fn is_voice(comp: &Comp, layer: &Layer) -> bool {
    comp.speech.as_ref().and_then(|m| m.layer) == Some(layer.id)
}

fn nearest(accents: &[(String, f64)], t: f64) -> Option<&(String, f64)> {
    accents.iter().min_by(|a, b| (a.1 - t).abs().total_cmp(&(b.1 - t).abs()))
}

/// The layer's first move with a speed peak (by its first key's time) against `accents`.
pub(crate) fn layer_beat(comp: &Comp, layer: &Layer, accents: &[(String, f64)]) -> Option<Beat> {
    if accents.is_empty() {
        return None;
    }
    let mut best: Option<(f64, f64)> = None;
    layer.props.walk("", &mut |_, pr| {
        for i in 0..pr.keys.len().saturating_sub(1) {
            let Some(k) = pr.keys.get(i) else { continue };
            let start = layer.comp_time(k.time).seconds();
            if best.is_some_and(|(s, _)| s <= start) {
                break;
            }
            if let Some((lt, _)) = lint::segment_peak(&pr.keys, i, pr.spatial) {
                best = Some((start, layer.comp_time(Tick::from_seconds_f64(lt)).seconds()));
                break;
            }
        }
    });
    let (key, peak) = best?;
    let (word, accent) = nearest(accents, peak)?.clone();
    let fps = comp.frame_rate.as_f64().max(1.0);
    Some(Beat { key, peak, word, accent, off: (peak - accent) * fps })
}

#[derive(Clone, Default)]
struct BeatCache {
    revision: u64,
    comp: u64,
    layers: std::collections::HashMap<u64, Option<Beat>>,
}

fn cached(ctx: &egui::Context, revision: u64, cid: u64, comp: &Comp, layer: &Layer) -> Option<Beat> {
    ctx.data_mut(|d| {
        let c = d.get_temp_mut_or_default::<BeatCache>(egui::Id::new("tl-beat-cache"));
        if c.revision != revision || c.comp != cid {
            *c = BeatCache { revision, comp: cid, layers: Default::default() };
        }
        c.layers.entry(layer.id.0).or_insert_with(|| layer_beat(comp, layer, &accents(comp))).clone()
    })
}

/// The off-beat dot on a keyed layer's bar, at its first key: green when the move's speed peak
/// is within [`ON_BEAT`] frames of the nearest stressed syllable, red otherwise. Hovering it says
/// by how much.
pub(crate) fn beat_dot(app: &mut dyn TimelineHost, p: &Painter, tm: TMap, cid: u64, comp: &Comp, layer: &Layer, row: Rect) {
    let ctx = p.ctx().clone();
    let Some(b) = cached(&ctx, app.session().revision, cid, comp, layer) else { return };
    let x = tm.x(b.key);
    if x < row.min.x - 4.0 || x > row.max.x + 4.0 {
        return;
    }
    let c = pos2(x, row.min.y + 4.0);
    let ok = b.off.abs() <= ON_BEAT;
    let colour = if ok { Color32::from_rgb(0x2f, 0xb3, 0x6f) } else { Color32::from_rgb(0xe5, 0x48, 0x4d) };
    p.circle_filled(c, 3.5, colour);
    p.circle_stroke(c, 3.5, egui::Stroke::new(1.0, Color32::from_black_alpha(160)));
    let fps = comp.frame_rate.as_f64();
    let note = format!("speed peak f{:.1} · {} f{:.0} · {:+.1} f", b.peak * fps, b.word, b.accent * fps, b.off);
    let hit = Rect::from_center_size(c, egui::vec2(10.0, 10.0));
    app.auto_mut().add(&format!("timeline.layer.{}.beat", layer.id.0), hit, &note);
    if let Some(ptr) = ctx.input(|i| i.pointer.hover_pos()).filter(|q| hit.contains(*q)) {
        super::drag_tip(&ctx, &app.tokens(), ptr, note);
    }
}

/// A key drag's syllable snap: the grabbed key's comp time that puts its move's peak on an
/// accent, and that accent's word.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snap {
    pub target: f64,
    pub word: String,
    pub accent: f64,
}

/// Snaps a key drag (`target`: where the grabbed key would go, comp seconds; `now`: where it is)
/// so the peak of its move lands on the nearest stressed syllable, when within [`SNAP_FRAMES`].
/// The move's peak offset from the key is measured on the keys as they are now (the selected
/// keys move together, so the offset holds).
// TODO(batch1-verify): with only one key of a move selected, moving it changes the move's shape; the offset is then approximate.
pub(crate) fn key_snap(app: &dyn TimelineHost, comp: &Comp, now: f64, target: f64) -> Option<Snap> {
    let acc = accents(comp);
    if acc.is_empty() {
        return None;
    }
    let fps = comp.frame_rate.as_f64().max(1.0);
    for k in &app.session().state.selected_keys {
        let Some(l) = comp.layer(k.layer) else { continue };
        if (l.comp_time(k.time).seconds() - now).abs() > 0.5 / fps {
            continue;
        }
        let Some(pr) = l.props.find(k.prop) else { continue };
        let Some(i) = pr.keys.iter().position(|x| x.time == k.time) else { continue };
        let pair = if i + 1 < pr.keys.len() { i } else { i.checked_sub(1)? };
        let Some((lt, _)) = lint::segment_peak(&pr.keys, pair, pr.spatial) else { continue };
        let po = l.comp_time(Tick::from_seconds_f64(lt)).seconds() - now;
        let (word, accent) = nearest(&acc, target + po)?.clone();
        let want = accent - po;
        if ((want - target) * fps).abs() <= SNAP_FRAMES {
            return Some(Snap { target: want, word, accent });
        }
        return None;
    }
    None
}

/// The snap line at the accent, labelled with its word, across the rows.
pub(crate) fn snap_line(p: &Painter, tm: TMap, rows: Rect, snap: &Snap) {
    let x = tm.x(snap.accent);
    if x < rows.min.x || x > rows.max.x {
        return;
    }
    let c = Color32::from_rgb(0xff, 0xc8, 0x3d);
    p.line_segment([pos2(x, rows.min.y), pos2(x, rows.max.y)], egui::Stroke::new(1.0, c));
    let g = p.layout_no_wrap(snap.word.clone(), Tokens::semibold(11.0), Color32::BLACK);
    let r = Rect::from_min_size(pos2(x + 3.0, rows.min.y + 2.0), g.size() + egui::vec2(8.0, 4.0));
    p.rect_filled(r, 3.0, c);
    p.galley(r.min + egui::vec2(4.0, 2.0), g, Color32::BLACK);
}

// ---------------------------------------------------------------- AX8: layer starts snap too

/// Where a dragged layer's in point goes (comp seconds) when the pointer asks for `desired_in`:
/// its first move's speed peak (or, without keys, its in point) snaps onto the nearest stressed
/// syllable within [`SNAP_FRAMES`]. `None` without a speech map (the Timeline's own drag then).
pub(crate) fn layer_drag_target(comp: &Comp, layer: &Layer, desired_in: f64) -> Option<(f64, Option<Snap>)> {
    if is_voice(comp, layer) {
        return None;
    }
    let acc = accents(comp);
    if acc.is_empty() {
        return None;
    }
    let fps = comp.frame_rate.as_f64().max(1.0);
    let cur_in = layer.in_point.seconds();
    // The point that lands on the syllable, as an offset from the in point.
    let po = layer_beat(comp, layer, &acc).map_or(0.0, |b| b.peak - cur_in);
    let Some((word, accent)) = nearest(&acc, desired_in + po).cloned() else { return Some((desired_in, None)) };
    let want = accent - po;
    if ((want - desired_in) * fps).abs() <= SNAP_FRAMES {
        return Some((want, Some(Snap { target: want, word, accent })));
    }
    Some((desired_in, None))
}
