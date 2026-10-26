//! Files dragged in from the OS (the Finder): where they are over the window, and where a drop
//! on the Timeline lands.
//!
//! winit 0.30 on macOS sends no pointer motion during an OS drag: its `NSDraggingDestination`
//! only queues hovered, dropped and cancelled files. egui's pointer is then gone (the cursor
//! left the window for the Finder) or stale, so a drop could not be placed: the Composition
//! viewer and the Timeline never saw it under them. [`drop_pointer`] asks the host for the OS
//! cursor ([`crate::Hooks::cursor_screen_position`], CGEvent on macOS) and turns it into UI
//! points, exactly on Retina, on monitors of different scales and with Settings UI zoom.
//!
//! The Timeline publishes its geometry every pass ([`publish_timeline`]); the window's drop
//! handler (`prefs_live::dropped_files`) places a drop with it as the panel places Project
//! items dropped there: in the layer stack where the line shows, and over the time graph
//! starting at the drop time (Shift: at the current time).

use effectcraft_engine::project::ItemId;
use effectcraft_engine::time::{FrameRate, Tick};
use egui::{Pos2, Rect, pos2};

use crate::EffectcraftApp;

/// A physical-pixel screen position as a point in the window's UI space, given the window's
/// content origin in UI points.
///
/// `px` must be in the pixels the window's inner position is in (winit: global points × the
/// window's native scale) and `pixels_per_point` is egui's zoomed one, as egui divides that
/// position by it to get `content_min`. A hostile scale counts as 1.
pub fn screen_px_to_ui(px: (i32, i32), pixels_per_point: f32, content_min: Pos2) -> Pos2 {
    let ppp = if pixels_per_point.is_finite() && pixels_per_point > 0.0 { pixels_per_point } else { 1.0 };
    pos2(px.0 as f32 / ppp - content_min.x, px.1 as f32 / ppp - content_min.y)
}

/// Where files dragged in from the OS are over the window, in UI points: the OS cursor through
/// the host hook (the hook scales by the window's native pixels per point, the result is divided
/// by the zoomed one), else the pointer egui sees (`None` while it is outside the window, so a
/// drop is never placed at a stale position).
pub fn drop_pointer(app: &EffectcraftApp, ctx: &egui::Context) -> Option<Pos2> {
    let (ppp, native, content_min) = ctx.input(|i| (i.pixels_per_point, i.viewport().native_pixels_per_point, i.viewport().inner_rect.map(|r| r.min)));
    let os = app
        .hooks
        .cursor_screen_position
        .as_ref()
        .zip(content_min)
        .and_then(|(cursor, min)| cursor(native.unwrap_or(ppp)).map(|px| screen_px_to_ui(px, ppp, min)))
        .filter(|p| p.is_finite());
    os.or_else(|| ctx.input(|i| i.pointer.hover_pos()))
}

/// Whether the OS is dragging files over the window or dropped some this pass.
pub fn files_present(ctx: &egui::Context) -> bool {
    ctx.input(|i| !i.raw.hovered_files.is_empty() || !i.raw.dropped_files.is_empty())
}

/// The Timeline panel's geometry in one pass, for placing files dropped from the OS.
#[derive(Clone, Debug)]
pub struct TimelineDropMap {
    /// The pass it was drawn in (`ctx.cumulative_pass_nr()`): a map from an earlier pass (a
    /// Timeline tab no longer shown) is not used.
    pub pass: u64,
    /// The composition the Timeline shows.
    pub comp: ItemId,
    /// The whole panel: a drop anywhere on it adds to the comp.
    pub panel: Rect,
    /// The layer rows' area (outline and time graph).
    pub rows_rect: Rect,
    /// Visible layer rows top to bottom, each with its 1-based stack index.
    pub layer_rows: Vec<(Rect, usize)>,
    /// The comp's layer count (a drop below with no rows goes under the last layer).
    pub layer_count: usize,
    /// Where the time graph starts; left of it a drop starts at the current time.
    pub graph_x0: f32,
    /// The Graph Editor is showing: no drop time (as for Project items).
    pub graph_on: bool,
    /// Time ↔ x: the x of `start_secs`, and pixels per second.
    pub x0: f32,
    pub start_secs: f64,
    pub pps: f64,
    pub frame_rate: FrameRate,
    pub duration: Tick,
    pub frame: Tick,
    /// The current time.
    pub now: Tick,
}

/// Where a drop on the Timeline lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineDrop {
    /// 1-based stack index the first new layer takes (`layer.addItem`'s `index`).
    pub index: usize,
    /// The In point over the time graph (`None`: the current time, as `layer.addItem` does).
    pub time: Option<Tick>,
    /// The y of the insertion line and, over the time graph, the x of the In point marker.
    pub line_y: f32,
    pub marker_x: Option<f32>,
}

impl TimelineDropMap {
    /// Where files dropped at `p` land, or `None` when `p` is not over the panel. `shift`: at the
    /// current time. Mirrors the Timeline's own drop of Project items.
    pub fn target(&self, p: Pos2, shift: bool) -> Option<TimelineDrop> {
        if !p.is_finite() || !self.panel.contains(p) {
            return None;
        }
        let at = self.layer_rows.iter().position(|(r, _)| p.y < r.center().y);
        let top = self.rows_rect.min.y;
        let line_y = match at {
            Some(i) => self.layer_rows.get(i).map_or(top, |(r, _)| r.min.y),
            None => self.layer_rows.last().map_or(top, |(r, _)| r.max.y),
        };
        let index = match at.and_then(|i| self.layer_rows.get(i)) {
            Some((_, ix)) => (*ix).max(1),
            None => self.layer_rows.last().map_or(self.layer_count, |(_, ix)| *ix).saturating_add(1),
        };
        let time = (p.x >= self.graph_x0 && !self.graph_on).then(|| {
            let t = if shift || !(self.pps.is_finite() && self.pps > 0.0) {
                self.now
            } else {
                Tick::from_seconds_f64(self.start_secs + f64::from(p.x - self.x0) / self.pps)
            };
            let last = (self.duration - self.frame).max(Tick::ZERO);
            self.frame_rate.snap_nearest(t).max(Tick::ZERO).min(last)
        });
        let marker_x = time.map(|t| self.x0 + ((t.seconds() - self.start_secs) * self.pps) as f32).filter(|x| x.is_finite());
        Some(TimelineDrop { index, time, line_y, marker_x })
    }
}

fn timeline_map_id() -> egui::Id {
    egui::Id::new("os-drop-timeline-map")
}

/// The Timeline's geometry this pass (called by the panel while it draws).
pub fn publish_timeline(ctx: &egui::Context, map: TimelineDropMap) {
    ctx.data_mut(|d| d.insert_temp(timeline_map_id(), map));
}

/// The Timeline's geometry if it was drawn this pass.
pub fn timeline_map(ctx: &egui::Context) -> Option<TimelineDropMap> {
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| d.get_temp::<TimelineDropMap>(timeline_map_id())).filter(|m| m.pass == pass)
}

/// Where the OS drop pointer would land on the Timeline of the active comp, if it is over it.
pub fn timeline_target(app: &EffectcraftApp, ctx: &egui::Context, p: Pos2) -> Option<TimelineDrop> {
    let map = timeline_map(ctx)?;
    if app.session.active_comp_id() != Some(map.comp) {
        return None;
    }
    map.target(p, ctx.input(|i| i.modifiers.shift))
}

/// While files are dragged over the Timeline: the insertion line and, over the time graph, the
/// In point marker, as for Project items dragged there. Returns the line's rectangle.
pub fn paint_timeline_target(ctx: &egui::Context, accent: egui::Color32, drop: &TimelineDrop) -> Option<Rect> {
    let map = timeline_map(ctx)?;
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("os-drop-timeline"))).with_clip_rect(map.rows_rect);
    let r = map.rows_rect;
    painter.line_segment([pos2(r.min.x, drop.line_y), pos2(r.max.x, drop.line_y)], egui::Stroke::new(2.0, accent));
    if let Some(x) = drop.marker_x {
        painter.line_segment([pos2(x, r.min.y), pos2(x, r.max.y)], egui::Stroke::new(1.0, accent));
    }
    Some(Rect::from_min_max(pos2(r.min.x, drop.line_y - 1.0), pos2(r.max.x, drop.line_y + 1.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;

    #[test]
    fn screen_pixels_become_window_points() {
        // Window content at (100, 50) points on a 2x display: pixel (1300, 900) is (550, 400).
        let p = screen_px_to_ui((1300, 900), 2.0, pos2(100.0, 50.0));
        assert_eq!((p.x, p.y), (550.0, 400.0));
    }

    #[test]
    fn a_hostile_scale_does_not_divide_by_zero() {
        for ppp in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let p = screen_px_to_ui((10, 20), ppp, Pos2::ZERO);
            assert_eq!((p.x, p.y), (10.0, 20.0), "ppp {ppp}");
        }
    }

    /// Rows Top (1), Middle (2), Bottom (3), 20 px high from y 100; the time graph from x 400 at
    /// 100 px per second from 0 s; 30 fps, 4 s, current time 1 s.
    fn map() -> TimelineDropMap {
        let row = |k: f32| Rect::from_min_size(pos2(0.0, 100.0 + 20.0 * k), vec2(1000.0, 20.0));
        TimelineDropMap {
            pass: 0,
            comp: ItemId(1),
            panel: Rect::from_min_max(pos2(0.0, 30.0), pos2(1000.0, 400.0)),
            rows_rect: Rect::from_min_max(pos2(0.0, 100.0), pos2(1000.0, 380.0)),
            layer_rows: vec![(row(0.0), 1), (row(1.0), 2), (row(2.0), 3)],
            layer_count: 3,
            graph_x0: 400.0,
            graph_on: false,
            x0: 406.0,
            start_secs: 0.0,
            pps: 100.0,
            frame_rate: FrameRate::new(30, 1),
            duration: Tick::from_seconds_f64(4.0),
            frame: FrameRate::new(30, 1).tick_of(1),
            now: Tick::from_seconds_f64(1.0),
        }
    }

    #[test]
    fn a_timeline_drop_goes_between_the_layers_and_starts_where_it_lands() {
        let m = map();
        // The lower half of Top, over the outline: above Middle (index 2), at the current time.
        let d = m.target(pos2(200.0, 115.0), false).unwrap();
        assert_eq!((d.index, d.time, d.line_y, d.marker_x), (2, None, 120.0, None));
        // Over the time graph at 1.5 s (x 406 + 150): there, snapped to a frame.
        let d = m.target(pos2(556.0, 105.0), false).unwrap();
        assert_eq!(d.index, 1);
        assert_eq!(d.time, Some(Tick::from_seconds_f64(1.5)));
        assert_eq!(d.marker_x, Some(556.0));
        // Shift: at the current time.
        assert_eq!(m.target(pos2(556.0, 105.0), true).unwrap().time, Some(Tick::from_seconds_f64(1.0)));
        // Below the last row: under Bottom.
        assert_eq!(m.target(pos2(200.0, 300.0), false).unwrap().index, 4);
        // Past the comp's end: its last frame.
        assert_eq!(m.target(pos2(990.0, 300.0), false).unwrap().time, Some(Tick::from_seconds_f64(4.0) - m.frame));
        // Off the panel: not a Timeline drop.
        assert_eq!(m.target(pos2(200.0, 500.0), false), None);
        assert_eq!(m.target(pos2(f32::NAN, 115.0), false), None);
    }

    #[test]
    fn an_empty_or_degenerate_timeline_still_gives_a_place() {
        let mut m = map();
        m.layer_rows.clear();
        m.layer_count = 0;
        m.pps = 0.0;
        m.duration = Tick::ZERO;
        let d = m.target(pos2(600.0, 200.0), false).unwrap();
        assert_eq!((d.index, d.line_y), (1, 100.0));
        // No pixels per second: the current time, clamped into the (empty) comp.
        assert_eq!(d.time, Some(Tick::ZERO));
        m.graph_on = true;
        assert_eq!(m.target(pos2(600.0, 200.0), false).unwrap().time, None, "the Graph Editor gives no drop time");
    }
}
