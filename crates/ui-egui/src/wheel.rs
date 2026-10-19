//! Radial wheels (batch 1, AX1): a ring of slices around the pointer, for the Graph wheel (hold
//! G), the Layer wheel (hold A), the card, paste and word wheels of the other panels.
//!
//! One wheel is open at a time. [`open`] puts it up, [`paint`] (once per frame, at the app's top
//! level, above the panels) draws it and reads the pointer, and [`take_pick`] hands the chosen
//! item's id to whoever opened it, once.
//!
//! - Hold and release: a wheel opened with `hold: Some(key)` picks the slice under the pointer
//!   when that key is released (nothing under it: the wheel closes).
//! - Click: a click on a slice picks it; a click in the middle or outside the ring closes it.
//! - Escape closes it without a pick.
//!
//! Every slice registers the automation id `wheel.<key>.<item id>` ([`register_automation`]), so
//! agents can click it. The state lives in the egui context (temp data), not in the document.

use egui::epaint::{PathShape, PathStroke};
use egui::{Align2, Color32, Pos2, Rect, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::theme::Tokens;

/// One slice of a wheel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WheelItem {
    /// What [`take_pick`] returns ("copy", "fastin"…).
    pub id: String,
    /// The slice's text ("Copy graph"). Two lines when it has several words.
    pub label: String,
    /// Shown under the wheel while the slice is hovered ("Copies the selected keys' graph").
    pub hint: String,
}

impl WheelItem {
    pub fn new(id: &str, label: &str, hint: &str) -> WheelItem {
        WheelItem { id: id.to_string(), label: label.to_string(), hint: hint.to_string() }
    }
}

/// Outer radius of the ring.
const R_OUT: f32 = 125.0;
/// Inner radius (the middle shows the wheel's name).
const R_IN: f32 = 34.0;
/// Closer than this to the middle nothing is hovered.
const DEAD: f32 = 30.0;
/// A click farther than this from the middle is outside the ring (closes the wheel).
const R_CLICK: f32 = R_OUT + 24.0;
/// Most slices a wheel shows.
const MAX_ITEMS: usize = 16;
/// Arc steps per slice when painting.
const ARC_STEPS: usize = 10;

const BG: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x21);
const BG_HOT: Color32 = Color32::from_rgb(0x2d, 0x8c, 0xff);
const EDGE: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x55);
const MIDDLE: Color32 = Color32::from_rgb(0x14, 0x14, 0x16);
const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe6, 0xea);
const ACCENT: Color32 = Color32::from_rgb(0x2d, 0x8c, 0xff);

#[derive(Clone, Debug)]
struct Open {
    key: String,
    center: Pos2,
    label: String,
    items: Vec<WheelItem>,
    hold: Option<egui::Key>,
    /// The pass it opened in: that pass's click (the button that opened it) is not a pick.
    opened_pass: u64,
    hover: Option<usize>,
    /// Each slice's hit rect (its label's middle), for automation.
    slots: Vec<Rect>,
}

#[derive(Clone, Debug, Default)]
struct State {
    open: Option<Open>,
    /// (wheel key, item id) picked and not yet taken.
    picks: Vec<(String, String)>,
}

fn state_id() -> egui::Id {
    egui::Id::new("ec-wheel-state")
}

fn load(ctx: &egui::Context) -> State {
    ctx.data(|d| d.get_temp::<State>(state_id())).unwrap_or_default()
}

fn store(ctx: &egui::Context, s: State) {
    ctx.data_mut(|d| d.insert_temp(state_id(), s));
}

/// Puts up the wheel `key` around `center` (moved in so the whole ring is on screen), with
/// `center_label` in the middle. `hold`: the key whose release picks. Replaces an open wheel.
pub fn open(ctx: &egui::Context, key: &str, center: Pos2, center_label: &str, items: Vec<WheelItem>, hold: Option<egui::Key>) {
    let screen = ctx.content_rect();
    let m = R_OUT + 10.0;
    let clamp = |v: f32, lo: f32, hi: f32| if hi < lo { (lo + hi) / 2.0 } else { v.clamp(lo, hi) };
    let c = pos2(clamp(center.x, screen.min.x + m, screen.max.x - m), clamp(center.y, screen.min.y + m, screen.max.y - m));
    let mut items = items;
    items.truncate(MAX_ITEMS);
    let mut s = load(ctx);
    s.open = Some(Open {
        key: key.to_string(),
        center: c,
        label: center_label.to_string(),
        items,
        hold,
        opened_pass: ctx.cumulative_pass_nr(),
        hover: None,
        slots: vec![],
    });
    store(ctx, s);
    ctx.request_repaint();
}

/// Whether a wheel is up.
pub fn is_open(ctx: &egui::Context) -> bool {
    load(ctx).open.is_some()
}

/// The open wheel's key, if one is up.
pub fn open_key(ctx: &egui::Context) -> Option<String> {
    load(ctx).open.map(|o| o.key)
}

/// Closes the open wheel without a pick.
pub fn close(ctx: &egui::Context) {
    let mut s = load(ctx);
    if s.open.take().is_some() {
        store(ctx, s);
        ctx.request_repaint();
    }
}

/// The id picked on wheel `key` since the last call, once.
pub fn take_pick(ctx: &egui::Context, key: &str) -> Option<String> {
    let mut s = load(ctx);
    let i = s.picks.iter().position(|(k, _)| k == key)?;
    let (_, id) = s.picks.remove(i);
    store(ctx, s);
    Some(id)
}

/// Picks `item` on the open wheel `key` as a click would (agents, tests). False when that wheel
/// isn't open or has no such item.
pub fn pick(ctx: &egui::Context, key: &str, item: &str) -> bool {
    let mut s = load(ctx);
    let ok = s.open.as_ref().is_some_and(|o| o.key == key && o.items.iter().any(|i| i.id == item));
    if ok {
        s.open = None;
        s.picks.push((key.to_string(), item.to_string()));
        store(ctx, s);
        ctx.request_repaint();
    }
    ok
}

/// The slice under `p`: by angle, slice 0 centred straight up, clockwise.
fn slice_at(center: Pos2, p: Pos2, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let d = p - center;
    if d.length() < DEAD {
        return None;
    }
    let step = 360.0 / n as f32;
    // Screen y grows downwards: atan2(dy, dx) is clockwise from +x; +90 puts 0 at the top.
    let ang = (d.y.atan2(d.x).to_degrees() + 90.0 + step / 2.0).rem_euclid(360.0);
    let i = (ang / step).floor() as usize;
    Some(i.min(n - 1))
}

fn polar(c: Pos2, r: f32, a: f32) -> Pos2 {
    pos2(c.x + r * a.cos(), c.y + r * a.sin())
}

/// Draws the open wheel and reads its input: call once per frame at the app's top level.
pub fn paint(ctx: &egui::Context) {
    let mut s = load(ctx);
    let Some(mut o) = s.open.take() else { return };
    let n = o.items.len();
    let ptr = ctx.input(|i| i.pointer.hover_pos());
    o.hover = ptr.and_then(|p| slice_at(o.center, p, n));
    // TODO(batch1-verify): a wheel opened by a button click: is that click's pass still `opened_pass` here?
    let fresh = ctx.cumulative_pass_nr() == o.opened_pass;
    // Escape: close, no pick.
    let escape = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if escape || n == 0 {
        store(ctx, s);
        ctx.request_repaint();
        return;
    }
    // Hold and release.
    if let Some(k) = o.hold
        && !fresh
        && ctx.input(|i| i.key_released(k) || !i.key_down(k))
    {
        if let Some(item) = o.hover.and_then(|h| o.items.get(h)) {
            s.picks.push((o.key.clone(), item.id.clone()));
        }
        store(ctx, s);
        ctx.request_repaint();
        return;
    }
    // Click.
    if !fresh && let Some(p) = ctx.input(|i| if i.pointer.primary_clicked() { i.pointer.interact_pos() } else { None }) {
        let dist = (p - o.center).length();
        if dist <= R_CLICK
            && let Some(item) = slice_at(o.center, p, n).and_then(|h| o.items.get(h))
        {
            s.picks.push((o.key.clone(), item.id.clone()));
        }
        store(ctx, s);
        ctx.request_repaint();
        return;
    }

    let c = o.center;
    // TODO(batch1-verify): check that the Area really keeps the Timeline from also taking a slice click.
    // An area over the ring takes the pointer, so the panels under it don't get its clicks
    // (egui hit-tests against the previous frame's areas).
    let side = R_CLICK * 2.0;
    let painter = egui::Area::new(egui::Id::new(("ec-wheel", o.key.as_str())))
        .order(egui::Order::Foreground)
        .fixed_pos(c - vec2(R_CLICK, R_CLICK))
        .constrain(false)
        .show(ctx, |ui| {
            let _ = ui.allocate_exact_size(vec2(side, side), egui::Sense::click());
            // The hint box sits below the area: paint unclipped.
            ui.painter().clone().with_clip_rect(ctx.content_rect())
        })
        .inner;
    // A soft disc behind the ring.
    painter.circle_filled(c, R_OUT + 6.0, Color32::from_black_alpha(120));
    let step = std::f32::consts::TAU / n as f32;
    o.slots.clear();
    for (i, item) in o.items.iter().enumerate() {
        let a0 = -std::f32::consts::FRAC_PI_2 + i as f32 * step - step / 2.0;
        let hot = o.hover == Some(i);
        let fill = if hot { BG_HOT } else { BG };
        // The annular slice as convex quads along the arc.
        for k in 0..ARC_STEPS {
            let u0 = a0 + step * k as f32 / ARC_STEPS as f32;
            let u1 = a0 + step * (k + 1) as f32 / ARC_STEPS as f32;
            let quad = vec![polar(c, R_IN, u0), polar(c, R_OUT, u0), polar(c, R_OUT, u1), polar(c, R_IN, u1)];
            painter.add(PathShape::convex_polygon(quad, fill, Stroke::new(0.5, fill)));
        }
        // Edges between slices.
        painter.line_segment([polar(c, R_IN, a0), polar(c, R_OUT, a0)], Stroke::new(1.0, EDGE));
        // The label, on two lines when it has several words.
        let mid = a0 + step / 2.0;
        let at = polar(c, (R_OUT + R_IN) / 2.0 + 4.0, mid);
        let words: Vec<&str> = item.label.split_whitespace().collect();
        let text = if words.len() > 1 {
            let k = words.len().div_ceil(2);
            format!("{}\n{}", words.get(..k).unwrap_or_default().join(" "), words.get(k..).unwrap_or_default().join(" "))
        } else {
            item.label.clone()
        };
        let r = painter.text(at, Align2::CENTER_CENTER, text, Tokens::ui(11.5), if hot { Color32::WHITE } else { TEXT });
        o.slots.push(r.expand(6.0));
    }
    // Outer and inner rims.
    painter.circle_stroke(c, R_OUT, Stroke::new(1.0, EDGE));
    painter.circle_filled(c, R_IN - 2.0, MIDDLE);
    painter.circle_stroke(c, R_IN - 2.0, Stroke::new(1.0, EDGE));
    painter.text(c, Align2::CENTER_CENTER, &o.label, Tokens::mono(13.0), ACCENT);
    // The hovered slice's hint under the ring.
    if let Some(item) = o.hover.and_then(|h| o.items.get(h)).filter(|i| !i.hint.is_empty()) {
        let g = painter.layout_no_wrap(item.hint.clone(), Tokens::ui(11.0), TEXT);
        let box_r = Rect::from_center_size(pos2(c.x, c.y + R_OUT + 22.0), g.size() + vec2(14.0, 8.0));
        painter.rect_filled(box_r, 4.0, MIDDLE);
        painter.add(PathShape::closed_line(vec![box_r.left_top(), box_r.right_top(), box_r.right_bottom(), box_r.left_bottom()], PathStroke::new(1.0, EDGE)));
        painter.galley(box_r.min + vec2(7.0, 4.0), g, TEXT);
    }
    s.open = Some(o);
    store(ctx, s);
    ctx.request_repaint();
}

/// Registers the open wheel's slices (`wheel.<key>.<item id>`) and its middle (`wheel.<key>`)
/// with the automation registry: call after [`paint`].
pub fn register_automation(ctx: &egui::Context, auto: &mut crate::automation::Registry) {
    let s = load(ctx);
    let Some(o) = s.open.as_ref() else { return };
    auto.add(&format!("wheel.{}", o.key), Rect::from_center_size(o.center, vec2(R_IN * 2.0, R_IN * 2.0)), &o.label);
    for (item, r) in o.items.iter().zip(&o.slots) {
        auto.add(&format!("wheel.{}.{}", o.key, item.id), *r, &item.label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_go_clockwise_from_the_top() {
        let c = pos2(200.0, 200.0);
        assert_eq!(slice_at(c, pos2(200.0, 100.0), 8), Some(0));
        assert_eq!(slice_at(c, pos2(300.0, 200.0), 8), Some(2));
        assert_eq!(slice_at(c, pos2(200.0, 300.0), 8), Some(4));
        assert_eq!(slice_at(c, pos2(100.0, 200.0), 8), Some(6));
        // The middle is dead.
        assert_eq!(slice_at(c, pos2(205.0, 205.0), 8), None);
        assert_eq!(slice_at(c, pos2(300.0, 200.0), 0), None);
    }

    #[test]
    fn open_pick_and_take_once() {
        let ctx = egui::Context::default();
        open(&ctx, "g", pos2(400.0, 400.0), "G", vec![WheelItem::new("copy", "Copy graph", ""), WheelItem::new("paste", "Paste graph", "")], None);
        assert!(is_open(&ctx));
        assert!(!pick(&ctx, "g", "nope"));
        assert!(pick(&ctx, "g", "paste"));
        assert!(!is_open(&ctx));
        assert_eq!(take_pick(&ctx, "g").as_deref(), Some("paste"));
        assert_eq!(take_pick(&ctx, "g"), None);
    }
}
