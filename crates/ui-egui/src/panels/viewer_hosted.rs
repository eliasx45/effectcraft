//! EMBED8: the Composition viewer's layer tools for a host that shows the comp somewhere else (FigCraft
//! draws a comp as a card on its canvas and hosts the Timeline under it).
//!
//! The host draws the frame; [`tools`] draws over it what the viewer draws over a frame for the
//! selected layers (mask and shape paths with their vertices and tangents, the anchor point for
//! the Rotation and Pan Behind tools, the tool's own feedback: shape ghost, Pen rubber band, text
//! caret and selection) and runs the viewer's tools on it:
//!
//! - **Rotation (W)** and **Pan Behind (Y)**: press a layer and drag, as in the viewer
//!   ([`viewer::begin_rotate`], [`viewer::begin_anchor`]); Shift snaps / keeps one axis, Alt moves
//!   the anchor point alone.
//! - **Shape tools (Q)**: drag a rectangle, rounded rectangle, ellipse, polygon or star: a mask on
//!   the selected footage / solid layer, else a shape (into the selected shape layer or a new one),
//!   as the viewer decides ([`viewer::create_shape`]).
//! - **Pen (G)**: click vertices, drag to pull tangents, click the first vertex to close, the last
//!   one of an open path to go on; Esc / Enter ends the path. A mask on the selected layer, a path
//!   in the selected shape layer, or a new shape layer ([`viewer::pen_press`]). Add / Delete /
//!   Convert Vertex click a path or vertex.
//! - **Selection (V)**: drag a path vertex (the selected ones move together) or a tangent handle;
//!   Alt-drag a vertex pulls new tangents, Alt-click converts it, Shift-click adds it to the
//!   selection. Layers themselves are the host's (it moves and scales them).
//! - **Type (Cmd+T)**: click for point text, drag for a paragraph box, click into a text layer to
//!   put the caret there, then type ([`super::viewer_text::hook`]: the same editing as in the
//!   viewer). [`edit_text_at`] starts editing a text layer from a host's own double-click.
//!
//! EMBED14 adds the rest of what the viewer shows over a frame:
//!
//! - **Guides and grids**: title/action safe, the grid, the proportional grid, the comp's guides
//!   and the 3D reference axes, each only while it is turned on (the View keys ' Cmd+' Cmd+;
//!   Cmd+R run in a host through `timeline::hosted::run`; [`grid_options`] /
//!   [`toggle_grid_option`] are the viewer's "Choose grid and guide options" menu for a host's
//!   own button). [`view_overlays`] draws them alone. With the Selection tool a guide drags (off
//!   the picture removes it); the rulers sit outside the picture's top and left edges
//!   ([`RULER`]) and guides are dragged out of them.
//! - **Motion paths**: the selected layers' Position motion path with its keys and spatial
//!   tangents (Settings ▸ Composition ▸ Motion Path decides how many keys show). The Selection
//!   tool drags keys and tangents (Alt pulls a key's tangents), the Pen slot adds, deletes and
//!   converts keys and bends the path between two keys, as in the viewer.
//! - **Mask Feather tool** (the Pen slot's last tool): drag from a mask's edge or its feather
//!   boundary to add a variable feather point and pull its radius, drag a point's handle to change
//!   it; the masks' feather boundaries show while the tool is on.
//!
//! Every edit is the viewer's own engine command, merged into one undo step per drag. Nothing else
//! is drawn: no labels, buttons or badges over the picture (the shape ghost goes uncaptioned).
//! The comp is mapped by a [`ViewerMap`] the host gives (where its picture is on screen); the
//! viewer's Hand/Zoom, region of interest, free transform, puppet and paint tools are not hosted.

use effectcraft_engine::project::LayerId;
use effectcraft_engine::render::EvalCtx;
use egui::{Color32, CursorIcon, Pos2, Rect, Sense, Stroke, vec2};
use serde_json::json;

use super::timeline::TimelineHost;
use super::viewer::{self, Gesture, ViewerMap};
use super::viewer_overlays::{self as ov, VertexHit};
use super::viewer_tools as vt;
use crate::layer_pointer::Lp;
use crate::state::Tool;

/// The viewer tools a host's comp has (others act as the Selection tool there).
pub fn hosted(tool: Tool) -> bool {
    matches!(
        tool,
        Tool::Selection
            | Tool::Rotate
            | Tool::PanBehind
            | Tool::Rectangle
            | Tool::RoundedRect
            | Tool::Ellipse
            | Tool::Polygon
            | Tool::Star
            | Tool::Pen
            | Tool::PenAdd
            | Tool::PenDelete
            | Tool::PenConvert
            | Tool::MaskFeather
            | Tool::Type
            | Tool::TypeVertical
    )
}

/// The tool [`tools`] acts with: the host's current one when hosted, else the Selection tool.
pub fn active(host: &dyn TimelineHost) -> Tool {
    let t = host.ui().tool;
    if hosted(t) { t } else { Tool::Selection }
}

/// How the host calls [`tools`].
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The widget id of the picture's interaction (one per picture).
    pub id: egui::Id,
    /// Space is held (the host pans): the tools sit the frame out.
    pub space_pan: bool,
}

/// What [`tools`] did with this frame's pointer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// The tools have the pointer over the picture this frame (a tool other than Selection, a
    /// drag under way, a text edit, or the pointer on a path vertex, a motion-path key or
    /// tangent, a guide or a ruler): the host leaves it alone.
    pub owns: bool,
    /// Something changed the project.
    pub edited: bool,
}

/// Thickness of the rulers (View ▸ Show Rulers) [`tools`] draws outside the picture's top and
/// left edges: a host's clip must leave this much room there.
pub const RULER: f32 = vt::RULER;

fn gesture_id() -> egui::Id {
    egui::Id::new("viewer-gesture")
}

fn drag_n_id() -> egui::Id {
    egui::Id::new("viewer-drag-n")
}

/// The vertex or tangent (`tangents`) handle within reach of `pos`, the last drawn first.
fn vertex_at(hits: &[VertexHit], pos: Pos2, tangents: bool) -> Option<VertexHit> {
    hits.iter().rev().filter(|h| h.tangent.is_some() == tangents).find(|h| h.pos.distance(pos) < viewer::VERTEX_GRAB).copied()
}

/// The pointer for `tool` over the picture (`over_vertex`: on a path point).
pub fn cursor(tool: Tool, over_vertex: bool) -> CursorIcon {
    match tool {
        Tool::Rotate => CursorIcon::Alias,
        Tool::PanBehind => CursorIcon::Move,
        Tool::Type | Tool::TypeVertical => CursorIcon::Text,
        Tool::Selection if over_vertex => CursorIcon::PointingHand,
        Tool::Selection => CursorIcon::Default,
        Tool::PenDelete | Tool::PenConvert if over_vertex => CursorIcon::PointingHand,
        _ => CursorIcon::Crosshair,
    }
}

/// The "Choose grid and guide options" menu of the viewer (Title/Action Safe, Proportional Grid,
/// Grid, Guides, Rulers, 3D Reference Axes): (entry, on) in its order, "-" a separator. A host
/// shows it from its own button and calls [`toggle_grid_option`] with the chosen index.
pub fn grid_options(host: &dyn TimelineHost) -> Vec<(String, bool)> {
    vt::grid_options(host)
}

/// Turn entry `index` of [`grid_options`] on or off, as the viewer's menu does.
pub fn toggle_grid_option(host: &mut dyn TimelineHost, index: usize) {
    vt::toggle_grid_option(host, index);
}

/// Whether any of [`grid_options`] shows something (the viewer's button is lit then).
pub fn grid_options_on(host: &dyn TimelineHost) -> bool {
    let v = &host.ui().viewer;
    let guides = v.guides && host.session().active_comp().is_some_and(|c| !c.guides.is_empty());
    v.grid || v.safe_margins || v.proportional_grid || guides || v.rulers
}

fn valid(map: &ViewerMap) -> bool {
    map.area.is_finite() && map.area.is_positive() && map.zoom.is_finite() && map.zoom > 0.0 && map.origin.x.is_finite() && map.origin.y.is_finite()
}

/// What the person turned on over the picture `map.area`, drawn without taking the pointer:
/// title/action safe, the grid, the proportional grid, the comp's guides (View menu, Choose grid
/// and guide options) and the 3D reference axes (Settings ▸ 3D, with 3D layers). Each only while
/// it is on; nothing otherwise. [`tools`] draws these itself; a host calls this while the tools
/// don't run (e.g. the Timeline is read-only).
pub fn view_overlays(host: &mut dyn TimelineHost, ui: &egui::Ui, map: ViewerMap) {
    if !valid(&map) {
        return;
    }
    let Some(cid) = host.session().active_comp_id() else { return };
    let Some(comp) = host.session().project.comp_arc(cid) else { return };
    let painter = ui.painter().clone();
    let pic = Rect::from_min_size(map.origin, vec2(map.comp[0] * map.zoom, map.comp[1] * map.zoom));
    vt::draw_view_guides(host, &painter, &comp, pic, map.area, map.zoom);
    let time = host.session().time();
    if host.session().prefs.three_d.show_reference_axes && comp.layers.iter().any(|l| l.is_3d() && l.is_active_at(time)) {
        let project = host.session().project.clone();
        let expr = host.session().expr.clone();
        let ectx = EvalCtx { project: &project, comp_id: cid, comp: &comp, time, expr: expr.as_deref(), footage: None };
        vt::draw_reference_axes(host, &painter, &ectx, map.area, [comp.width as f32, comp.height as f32]);
    }
}

/// One frame of the viewer tools over the comp picture `map.area` (window coordinates of `ui`'s
/// layer). Draws with `ui`'s painter (clip it to where the picture shows, plus [`RULER`] above
/// and left of it while the rulers are on). Never panics on junk maps: a non-finite or empty one
/// does nothing.
pub fn tools(host: &mut dyn TimelineHost, ui: &mut egui::Ui, map: ViewerMap, opts: Options) -> Frame {
    let mut out = Frame::default();
    if !valid(&map) {
        return out;
    }
    let before = host.session().revision;
    let Some(cid) = host.session().active_comp_id() else { return out };
    let Some(comp) = host.session().project.comp_arc(cid) else { return out };
    let project = host.session().project.clone();
    let expr = host.session().expr.clone();
    let time = host.session().time();
    let ectx = EvalCtx { project: &project, comp_id: cid, comp: &comp, time, expr: expr.as_deref(), footage: None };
    let ctx = ui.ctx().clone();
    let painter = ui.painter().clone();
    let tool = active(host);
    let gid = gesture_id();
    let lp = Lp::of(ui);
    let hover = lp.pos(ctx.input(|i| i.pointer.hover_pos())).filter(|h| map.area.contains(*h));
    let mods = ctx.input(|i| i.modifiers);

    // EMBED14: what the person turned on (guides, grids, safe areas, 3D axes), under the paths.
    view_overlays(host, ui, map);
    // The rulers, outside the picture's top and left edges: guides are dragged out of them.
    let mut on_ruler = false;
    if host.ui().viewer.rulers {
        let outer = Rect::from_min_max(map.area.min - vec2(RULER, RULER), map.area.max);
        vt::rulers(host, ui, &map, outer, map.area);
        on_ruler = lp.pos(ctx.input(|i| i.pointer.hover_pos())).is_some_and(|h| outer.contains(h) && !map.area.contains(h));
    }

    // A Pen path in progress ends when the tool changes or with Esc / Enter.
    let finish = ctx.input(|i| i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Enter)) && !ctx.egui_wants_keyboard_input();
    if tool != Tool::Pen || finish {
        ui.data_mut(|d| d.remove::<(LayerId, u64)>(viewer::pen_id()));
    }
    let pen = ui.data(|d| d.get_temp::<(LayerId, u64)>(viewer::pen_id()));

    // The selected layers' motion paths, paths and (Rotation / Pan Behind) anchor points.
    let t = host.tokens();
    let selected = host.session().state.selected_layers.clone();
    let sel_vertices = host.session().state.selected_vertices.clone();
    let show_masks = host.ui().viewer.show_masks;
    let layer_controls = host.ui().viewer.show_layer_controls;
    let label_handles = host.session().prefs.appearance.use_label_color_for_handles;
    let point = host.session().prefs.general.path_point_size as f32 + 1.0;
    let mut hits: Vec<VertexHit> = Vec::new();
    let mut marks: Vec<ov::VertexMark> = Vec::new();
    let mut paths: Vec<ov::PathInfo> = Vec::new();
    let mut key_hits: Vec<ov::KeyHit> = Vec::new();
    let mut segments: Vec<ov::MotionSegment> = Vec::new();
    let mut frames: Vec<ov::PathFrame> = Vec::new();
    for l in comp.layers.iter().filter(|l| selected.contains(&l.id) && l.is_active_at(time) && l.switches.video) {
        let col = if label_handles { t.label(l.label) } else { t.accent };
        // EMBED14: the position motion path (keys and spatial tangents drag), as the viewer
        // shows it (Settings ▸ Composition ▸ Motion Path: how many keyframes).
        if layer_controls {
            ov::motion_path(host, &painter, &map, &ectx, l, col, &mut key_hits, &mut segments, &mut frames);
        }
        if show_masks {
            ov::draw_paths(host, &painter, &map, &ectx, l, col, &sel_vertices, pen, hover, &mut hits, &mut marks, &mut paths);
        }
        if matches!(tool, Tool::Rotate | Tool::PanBehind)
            && let Some(tr) = l.transform()
        {
            let a = ectx.v3(l, tr, "anchor", [0.0; 3]);
            let c = viewer::box_l2c(&ectx, l).apply(effectcraft_engine::geom::vec2(a[0], a[1]));
            let s = map.to_screen([c.x, c.y]);
            if s.x.is_finite() && s.y.is_finite() {
                let stroke = Stroke::new(1.0, Color32::WHITE);
                painter.circle_stroke(s, 5.0, stroke);
                painter.line_segment([s - vec2(9.0, 0.0), s + vec2(9.0, 0.0)], stroke);
                painter.line_segment([s - vec2(0.0, 9.0), s + vec2(0.0, 9.0)], stroke);
            }
        }
    }
    ov::draw_vertex_marks(&painter, &marks, point);
    // EMBED14: the Mask Feather tool shows the masks' feather boundaries.
    if tool == Tool::MaskFeather {
        ov::draw_feather_boundaries(&painter, &map, &ectx, &paths);
    }

    let gesture: Option<Gesture> = ui.data(|d| d.get_temp(gid));
    let editing_text = host.session().state.text_edit.is_some();
    let over_vertex = hover.is_some_and(|h| vertex_at(&hits, h, true).or_else(|| vertex_at(&hits, h, false)).is_some());
    let over_key = hover.is_some_and(|h| key_hits.iter().any(|k| k.pos.distance(h) < 6.0));
    let over_guide = hover.and_then(|h| vt::guide_at(host, &comp, &map, h));
    let held = ctx.input(|i| i.pointer.any_down()) && ui.ctx().is_being_dragged(opts.id);
    out.owns = !opts.space_pan
        && (tool != Tool::Selection
            || gesture.is_some()
            || editing_text
            || over_vertex
            || over_key
            || over_guide.is_some()
            || pen.is_some()
            || held
            || on_ruler);
    // On a ruler (no drag under way) the ruler has the pointer, not the picture.
    if !out.owns || (on_ruler && gesture.is_none() && !held) {
        out.edited = host.session().revision != before;
        return out;
    }
    let resp = ui.interact(map.area, opts.id, Sense::click_and_drag());
    if resp.hovered() || resp.dragged() {
        let icon = match over_guide {
            Some((_, true)) if tool == Tool::Selection => CursorIcon::ResizeHorizontal,
            Some((_, false)) if tool == Tool::Selection => CursorIcon::ResizeVertical,
            _ if tool == Tool::Selection && over_key => CursorIcon::Move,
            _ => cursor(tool, over_vertex),
        };
        ctx.set_cursor_icon(icon);
    }

    // Pen: each press places a vertex (see `viewer::pen_press`), except on a motion path.
    if tool == Tool::Pen
        && resp.hovered()
        && ctx.input(|i| i.pointer.primary_pressed())
        && let Some(pos) = lp.pos(ctx.input(|i| i.pointer.press_origin()))
        && matches!(ov::pen_target(&key_hits, &segments, &frames, pos), ov::PenTarget::None)
    {
        viewer::pen_press(host, ui, &ectx, &map, &paths, None, pos, mods);
    }
    // A Pen click (press and release, no drag) ends its tangent gesture on release.
    if ctx.input(|i| i.pointer.primary_released()) && matches!(ui.data(|d| d.get_temp::<Gesture>(gid)), Some(Gesture::Pen { .. })) && !resp.drag_stopped() {
        ui.data_mut(|d| d.remove::<Gesture>(gid));
    }
    // Text editing owns its clicks and drags (Type tool, a text edit under way).
    let text_owns = super::viewer_text::hook(host, ui, &painter, &map, &ectx, &resp, opts.space_pan);

    if resp.drag_started()
        && !text_owns
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let press = lp.pos(ctx.input(|i| i.pointer.press_origin())).unwrap_or(pos);
        let cpt = map.to_comp(press);
        let target = ov::pen_target(&key_hits, &segments, &frames, press);
        let g = match tool {
            Tool::Rotate => viewer::begin_rotate(host, &ectx, &comp, &map, cpt, pos, mods.shift),
            Tool::PanBehind => viewer::begin_anchor(host, &ectx, &map, cpt, press, mods.alt),
            t if t.is_shape() => Some(Gesture::Create { tool: t, start: cpt }),
            // Motion paths, as in the viewer: Alt (or Convert Vertex) pulls a key's tangents out,
            // a key or tangent drags, the Pen bends the path between two keys.
            Tool::PenConvert | Tool::Pen | Tool::Selection if (tool == Tool::PenConvert || mods.alt) && ov::motion_key_at(&key_hits, press).is_some() => {
                ov::motion_key_at(&key_hits, press).and_then(|h| {
                    h.p2c
                        .inverse()
                        .map(|inv| Gesture::Overlay(ov::Drag::KeyTangent { layer: h.layer, prop: h.prop, time: h.time, out: true, key: h.value, inv }))
                })
            }
            Tool::Pen | Tool::PenAdd | Tool::PenConvert if matches!(target, ov::PenTarget::Key(_) | ov::PenTarget::Tangent) => {
                ov::begin_key_drag(host, &key_hits, press, cpt, mods.shift).map(Gesture::Overlay)
            }
            Tool::Pen | Tool::PenConvert if matches!(target, ov::PenTarget::AddFrame(_) | ov::PenTarget::PathFull) => {
                ov::begin_segment_bend(host, &segments, press, cpt).map(Gesture::Overlay)
            }
            Tool::Selection if key_hits.iter().any(|h| h.pos.distance(press) < 6.0) => {
                ov::begin_key_drag(host, &key_hits, press, cpt, mods.shift).map(Gesture::Overlay)
            }
            Tool::Selection if vt::guide_at(host, &comp, &map, press).is_some() => {
                vt::guide_at(host, &comp, &map, press).map(|(index, vertical)| Gesture::Overlay(ov::Drag::Guide { index, vertical }))
            }
            Tool::MaskFeather => ov::begin_feather(host, &ectx, &paths, &map, press).map(Gesture::Overlay),
            Tool::Selection | Tool::PenConvert if (mods.alt || tool == Tool::PenConvert) && vertex_at(&hits, press, false).is_some() => {
                vertex_at(&hits, press, false).map(|h| Gesture::Pen {
                    layer: h.layer,
                    mask: h.mask,
                    index: h.index,
                    vertex: h.vertex,
                    inv: h.l2c.inverse().unwrap_or(effectcraft_engine::geom::Mat3::IDENTITY),
                    press,
                })
            }
            Tool::Selection if vertex_at(&hits, press, true).is_some() => vertex_at(&hits, press, true).map(|h| Gesture::Tangent {
                layer: h.layer,
                mask: h.mask,
                index: h.index,
                out: h.tangent == Some(true),
                vertex: h.vertex,
                inv: h.l2c.inverse().unwrap_or(effectcraft_engine::geom::Mat3::IDENTITY),
            }),
            Tool::Selection => vertex_at(&hits, press, false).map(|h| {
                let picked = sel_vertices.iter().any(|v| v.layer == h.layer && v.mask == h.mask && v.index == h.index);
                if !picked {
                    let me = json!({"layer": h.layer.0, "mask": h.mask, "index": h.index});
                    let _ = host.session_mut().execute("mask.selectVertices", json!({"vertices": [me], "add": mods.shift}));
                }
                let src = h.l2c.apply(effectcraft_engine::geom::vec2(h.vertex[0], h.vertex[1]));
                Gesture::Vertices { start: cpt, src: [src.x, src.y], applied: [0.0; 2], layer: h.layer }
            }),
            _ => None,
        };
        if let Some(g) = g {
            ui.data_mut(|d| d.insert_temp(gid, g));
        }
    }
    let gesture: Option<Gesture> = ui.data(|d| d.get_temp(gid));
    if let (Some(g), Some(pos)) = (gesture.clone(), resp.interact_pointer_pos()) {
        let merge = format!("viewer-drag-{}", ui.data(|d| d.get_temp::<u64>(drag_n_id()).unwrap_or(0)));
        match g {
            Gesture::Overlay(d) => overlay_drag(host, ui, &ectx, &map, d, pos, mods, &merge),
            g => viewer::update_tool_gesture(host, ui, &ectx, &map, &painter, g, pos, mods, &merge, false),
        }
        // The snap feedback of this frame's drag (the target highlighted, where it landed).
        vt::draw_snap(&ctx, &painter, &map, &ectx);
    }
    if resp.drag_stopped() {
        match (gesture, resp.interact_pointer_pos()) {
            (Some(Gesture::Create { tool, start }), Some(pos)) => viewer::create_shape(host, tool, start, map.to_comp(pos), mods),
            // A guide dragged off the picture is removed, as in the viewer.
            (Some(Gesture::Overlay(ov::Drag::Guide { index, .. })), Some(pos)) if !map.area.expand(40.0).contains(pos) => {
                let _ = host.session_mut().execute("view.removeGuide", json!({"index": index}));
            }
            _ => {}
        }
        ui.data_mut(|d| {
            d.remove::<Gesture>(gid);
            let n: u64 = d.get_temp(drag_n_id()).unwrap_or(0);
            d.insert_temp(drag_n_id(), n.wrapping_add(1));
        });
    }
    if resp.clicked()
        && !text_owns
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let motion = Motion { keys: &key_hits, segments: &segments, frames: &frames };
        click(host, &ectx, &map, &hits, &paths, &motion, tool, pos, mods);
    }
    out.edited = host.session().revision != before;
    if out.edited {
        ctx.request_repaint();
    }
    out
}

/// Advance a motion-path, guide or Mask Feather drag (`d`) to screen point `pos`.
#[allow(clippy::too_many_arguments)]
fn overlay_drag(host: &mut dyn TimelineHost, ui: &egui::Ui, ectx: &EvalCtx, map: &ViewerMap, d: ov::Drag, pos: Pos2, mods: egui::Modifiers, merge: &str) {
    let cpt = map.to_comp(pos);
    if !cpt.iter().all(|v| v.is_finite()) {
        return;
    }
    match d {
        ov::Drag::Guide { index, vertical } => {
            let at = if vertical { cpt[0] } else { cpt[1] }.round();
            let _ = host.session_mut().execute("view.moveGuide", json!({"index": index, "position": at, "merge": merge}));
        }
        ov::Drag::Roi { .. } => {}
        d => {
            let ctx = ui.ctx().clone();
            let mut snapper = |a: &dyn TimelineHost, p: [f64; 2], l: LayerId| vt::snap(a, &ctx, ectx, map, &[l], &[p], mods);
            if let Some(nd) = ov::update(host, &d, cpt, pos, mods, merge, map.zoom, &mut snapper) {
                ui.data_mut(|dd| dd.insert_temp(gesture_id(), Gesture::Overlay(nd)));
            }
        }
    }
}

/// The selected layers' motion paths drawn this frame.
struct Motion<'a> {
    keys: &'a [ov::KeyHit],
    segments: &'a [ov::MotionSegment],
    frames: &'a [ov::PathFrame],
}

/// A click (no drag) at screen point `pos` with `tool`.
#[allow(clippy::too_many_arguments)]
fn click(
    host: &mut dyn TimelineHost,
    ectx: &EvalCtx,
    map: &ViewerMap,
    hits: &[VertexHit],
    paths: &[ov::PathInfo],
    motion: &Motion,
    tool: Tool,
    pos: Pos2,
    mods: egui::Modifiers,
) {
    let vertex = vertex_at(hits, pos, false).map(|h| json!({"layer": h.layer.0, "mask": h.mask, "index": h.index}));
    if !matches!(tool, Tool::Selection | Tool::Pen | Tool::PenAdd | Tool::PenDelete | Tool::PenConvert | Tool::MaskFeather | Tool::Type | Tool::TypeVertical) {
        // Rotation, Pan Behind and the shape tools: a click selects the layer under it (Shift
        // toggles it), or nothing.
        if viewer::pick(host, ectx, map.to_comp(pos), mods.shift).is_none() {
            let _ = host.session_mut().execute("edit.deselectAll", json!({}));
        }
        return;
    }
    // EMBED14: on a motion path, as in the viewer: the Pen deletes a key (Alt converts it) or adds
    // one on a free frame; Delete / Convert Vertex delete / convert a key; Alt-click with the
    // Selection tool converts it.
    let key = ov::motion_key_at(motion.keys, pos);
    match tool {
        Tool::Pen | Tool::PenAdd if !matches!(ov::pen_target(motion.keys, motion.segments, motion.frames, pos), ov::PenTarget::None) => {
            match ov::pen_target(motion.keys, motion.segments, motion.frames, pos) {
                ov::PenTarget::Key(h) if tool == Tool::Pen => ov::edit_motion_key(host, h, !mods.alt),
                ov::PenTarget::AddFrame(f) => ov::insert_motion_key(host, (f.layer, f.prop, f.time)),
                _ => {}
            }
            return;
        }
        Tool::PenDelete | Tool::PenConvert if key.is_some() => {
            if let Some(h) = key {
                ov::edit_motion_key(host, h, tool == Tool::PenDelete);
            }
            return;
        }
        Tool::Selection if mods.alt && key.is_some() => {
            if let Some(h) = key {
                ov::edit_motion_key(host, h, false);
            }
            return;
        }
        Tool::PenAdd if ov::segment_at(paths, map, pos, 6.0).is_none() => {
            if let Some(f) = ov::motion_frame_at(motion.frames, motion.keys, pos) {
                viewer::add_motion_key(host, f);
            }
            return;
        }
        _ => {}
    }
    let s = host.session_mut();
    let r = match tool {
        // The Pen placed its vertex on the press; Type handled its own click; the Mask Feather
        // tool works by dragging.
        Tool::Pen | Tool::Type | Tool::TypeVertical | Tool::MaskFeather => return,
        Tool::PenAdd => match ov::segment_at(paths, map, pos, 6.0) {
            Some((l, m, seg, t)) => s.execute("mask.insertVertex", json!({"layer": l.0, "mask": m, "segment": seg, "t": t})),
            None => return,
        },
        Tool::PenDelete => match vertex {
            Some(v) => s.execute("mask.deleteVertices", json!({"vertices": [v]})),
            None => return,
        },
        Tool::PenConvert => match vertex {
            Some(v) => s.execute("mask.convertVertex", v),
            None => return,
        },
        Tool::Selection if mods.alt && vertex.is_some() => match vertex {
            Some(v) => s.execute("mask.convertVertex", v),
            None => return,
        },
        Tool::Selection => match vertex {
            Some(v) => s.execute("mask.selectVertices", json!({"vertices": [v], "add": mods.shift, "toggle": mods.shift})),
            None => return,
        },
        _ => return,
    };
    if let Err(e) = r {
        host.ui_mut().status = e.to_string();
    }
}

/// Start editing the text layer under screen point `pos` on the picture `map` shows (a host's
/// double-click with the Selection tool, as the viewer's). Returns whether there was one.
pub fn edit_text_at(host: &mut dyn TimelineHost, ctx: &egui::Context, map: ViewerMap, pos: Pos2) -> bool {
    if !map.area.is_finite() || !map.zoom.is_finite() || map.zoom <= 0.0 || !pos.x.is_finite() || !pos.y.is_finite() || !map.area.contains(pos) {
        return false;
    }
    let Some(cid) = host.session().active_comp_id() else { return false };
    let Some(comp) = host.session().project.comp_arc(cid) else { return false };
    let project = host.session().project.clone();
    let expr = host.session().expr.clone();
    let ectx = EvalCtx { project: &project, comp_id: cid, comp: &comp, time: host.session().time(), expr: expr.as_deref(), footage: None };
    super::viewer_text::edit_at(host, ctx, &map, &ectx, pos)
}

/// The comp pixel at screen point `p` on the picture `map` shows (`None` off it or for junk).
pub fn comp_point(map: &ViewerMap, p: Pos2) -> Option<[f64; 2]> {
    (map.area.is_finite() && map.zoom.is_finite() && map.zoom > 0.0 && map.area.contains(p)).then(|| map.to_comp(p)).filter(|c| c.iter().all(|v| v.is_finite()))
}

/// A [`ViewerMap`] for a comp `size` (pixels) shown filling `pic` (screen points, the comp's
/// aspect). `None` for an empty or non-finite picture or comp.
pub fn map_for(pic: Rect, size: [u32; 2]) -> Option<ViewerMap> {
    if !pic.is_finite() || pic.width() <= 0.0 || pic.height() <= 0.0 || size[0] == 0 || size[1] == 0 {
        return None;
    }
    let zoom = pic.width() / size[0] as f32;
    (zoom.is_finite() && zoom > 0.0).then_some(ViewerMap { origin: pic.min, zoom, comp: [size[0] as f32, size[1] as f32], area: pic })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use effectcraft_engine::Session;
    use effectcraft_engine::project::{LayerId, LayerSource};
    use egui::{Event, PointerButton, pos2};
    use serde_json::{Value, json};

    use super::*;
    use crate::automation::Registry;
    use crate::panels::timeline::{TimelineParts, hosted as th};
    use crate::state::UiState;
    use crate::theme::Tokens;

    /// A host with only what the trait requires, as FigCraft's card is: a 320×180 comp with a
    /// full-size Plate and a 40×40 Box in the middle (box 140,70 – 180,110), shown 1:1 at the
    /// window's origin.
    struct Mini {
        session: Session,
        ui: UiState,
        auto: Registry,
        tokens: Tokens,
    }

    impl TimelineHost for Mini {
        fn session(&self) -> &Session {
            &self.session
        }
        fn session_mut(&mut self) -> &mut Session {
            &mut self.session
        }
        fn ui(&self) -> &UiState {
            &self.ui
        }
        fn ui_mut(&mut self) -> &mut UiState {
            &mut self.ui
        }
        fn auto(&self) -> &Registry {
            &self.auto
        }
        fn auto_mut(&mut self) -> &mut Registry {
            &mut self.auto
        }
        fn tokens(&self) -> Tokens {
            self.tokens
        }
        fn parts(&mut self) -> TimelineParts<'_> {
            TimelineParts { session: &mut self.session, ui: &mut self.ui, auto: &mut self.auto, tokens: self.tokens }
        }
        fn invoke(&mut self, ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
            if let Some(r) = th::run(self, ctx, id, &params) {
                return r;
            }
            self.session.execute(id, params).map_err(|e| e.to_string())
        }
    }

    fn mini() -> (Mini, LayerId, LayerId) {
        let mut session = Session::default();
        session.execute("comp.new", json!({"name": "Card", "width": 320, "height": 180, "duration": 4})).unwrap();
        let id = |v: Value| LayerId(v["layer"].as_u64().unwrap());
        let plate = id(session.execute("layer.newSolid", json!({"name": "Plate", "color": "#406080", "width": 320, "height": 180})).unwrap());
        let bx = id(session.execute("layer.newSolid", json!({"name": "Box", "color": "#e04020", "width": 40, "height": 40})).unwrap());
        session.execute("edit.deselectAll", json!({})).unwrap();
        // Snapping off: the drags below land exactly where they are dragged.
        session.state.snapping = false;
        th::install(&mut session);
        let host = Mini { session, ui: UiState::default(), auto: Registry::default(), tokens: Tokens::for_kind(crate::theme::ThemeKind::Dark) };
        (host, plate, bx)
    }

    fn map() -> ViewerMap {
        map_for(Rect::from_min_size(Pos2::ZERO, vec2(320.0, 180.0)), [320, 180]).unwrap()
    }

    fn frame(ctx: &egui::Context, h: &mut Mini, events: Vec<Event>) -> Frame {
        frame_on(ctx, h, events, map())
    }

    /// One frame of the tools with the picture at `m` (the card at another canvas zoom / place).
    fn frame_on(ctx: &egui::Context, h: &mut Mini, events: Vec<Event>, m: ViewerMap) -> Frame {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), events, ..Default::default() };
        let mut out = Frame::default();
        let _ = ctx.run_ui(input, |ui| {
            h.auto.begin_frame();
            out = tools(h, ui, m, Options { id: egui::Id::new("card-tools"), space_pan: false });
        });
        out
    }

    /// A drag along `path` on the picture at `m`.
    fn drag_on(ctx: &egui::Context, h: &mut Mini, path: &[Pos2], m: ViewerMap) {
        let first = path[0];
        frame_on(ctx, h, vec![Event::PointerMoved(first)], m);
        frame_on(ctx, h, vec![button(first, true)], m);
        for p in &path[1..] {
            frame_on(ctx, h, vec![Event::PointerMoved(*p)], m);
        }
        let last = *path.last().unwrap();
        frame_on(ctx, h, vec![button(last, false)], m);
        frame_on(ctx, h, vec![], m);
    }

    /// How many shapes a frame paints: with the tools (`tools`) or without them (the baseline).
    fn painted(ctx: &egui::Context, h: &mut Mini, with_tools: bool) -> usize {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))), ..Default::default() };
        let out = ctx.run_ui(input, |ui| {
            h.auto.begin_frame();
            if with_tools {
                let _ = tools(h, ui, map(), Options { id: egui::Id::new("card-tools"), space_pan: false });
            }
        });
        out.shapes.len()
    }

    /// The Box's Position keys: 160,90 at 0 s and 260,90 at 2 s (Box selected, time 0).
    fn keyed_box(h: &mut Mini, bx: LayerId) {
        h.session.execute("prop.addKey", json!({"layer": bx.0, "path": "transform/position", "time": 0.0, "value": [160.0, 90.0, 0.0]})).unwrap();
        h.session.execute("prop.addKey", json!({"layer": bx.0, "path": "transform/position", "time": 2.0, "value": [260.0, 90.0, 0.0]})).unwrap();
        h.session.execute("layer.select", json!({"layers": [bx.0]})).unwrap();
        h.session.execute("keys.deselectAll", json!({})).ok();
    }

    fn position_keys(h: &Mini, bx: LayerId) -> Vec<effectcraft_engine::keyframe::Keyframe> {
        let c = h.session.active_comp().unwrap();
        c.layer(bx).unwrap().props.prop("transform/position").unwrap().keys.clone()
    }

    fn feather_points(h: &Mini, l: LayerId) -> Vec<effectcraft_engine::keyframe::FeatherPoint> {
        let c = h.session.active_comp().unwrap();
        let layer = c.layer(l).unwrap();
        let g = layer.masks().unwrap().groups().next().unwrap();
        let v = layer.props.find_group(g.uid).unwrap().get("path").unwrap().value_at(layer.layer_time(h.session.time()));
        v.as_path().cloned().unwrap().feather
    }

    fn button(at: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE }
    }

    /// Hover, press at `path[0]`, move through the rest, release: a drag.
    fn drag(ctx: &egui::Context, h: &mut Mini, path: &[Pos2]) {
        let first = path[0];
        frame(ctx, h, vec![Event::PointerMoved(first)]);
        frame(ctx, h, vec![button(first, true)]);
        for p in &path[1..] {
            frame(ctx, h, vec![Event::PointerMoved(*p)]);
        }
        let last = *path.last().unwrap();
        frame(ctx, h, vec![button(last, false)]);
        frame(ctx, h, vec![]);
    }

    fn click(ctx: &egui::Context, h: &mut Mini, at: Pos2) {
        frame(ctx, h, vec![Event::PointerMoved(at)]);
        frame(ctx, h, vec![button(at, true)]);
        frame(ctx, h, vec![button(at, false)]);
        frame(ctx, h, vec![]);
    }

    fn v3(h: &Mini, l: LayerId, path: &str) -> [f64; 3] {
        let c = h.session.active_comp().unwrap();
        let layer = c.layer(l).unwrap();
        layer.props.prop(path).unwrap().value_at(layer.layer_time(h.session.time())).as_vec3()
    }

    fn f(h: &Mini, l: LayerId, path: &str) -> f64 {
        let c = h.session.active_comp().unwrap();
        let layer = c.layer(l).unwrap();
        layer.props.prop(path).unwrap().value_at(layer.layer_time(h.session.time())).as_f64()
    }

    fn masks(h: &Mini, l: LayerId) -> usize {
        h.session.active_comp().unwrap().layer(l).unwrap().masks().map_or(0, |m| m.groups().count())
    }

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        crate::theme::install_panel_fonts(&ctx);
        ctx
    }

    #[test]
    fn map_for_rejects_junk_and_round_trips() {
        assert!(map_for(Rect::NOTHING, [100, 100]).is_none());
        assert!(map_for(Rect::from_min_size(Pos2::ZERO, vec2(100.0, 50.0)), [0, 10]).is_none());
        assert!(map_for(Rect::from_min_size(Pos2::new(f32::NAN, 0.0), vec2(100.0, 50.0)), [10, 10]).is_none());
        let m = map_for(Rect::from_min_size(Pos2::new(10.0, 20.0), vec2(192.0, 108.0)), [1920, 1080]).unwrap();
        let c = comp_point(&m, Pos2::new(106.0, 74.0)).unwrap();
        assert!((c[0] - 960.0).abs() < 1e-3 && (c[1] - 540.0).abs() < 1e-3, "{c:?}");
        assert!(comp_point(&m, Pos2::new(0.0, 0.0)).is_none());
    }

    #[test]
    fn only_the_card_tools_are_hosted() {
        for t in [Tool::Rotate, Tool::PanBehind, Tool::Ellipse, Tool::Pen, Tool::MaskFeather, Tool::Type, Tool::Selection] {
            assert!(hosted(t), "{t:?}");
        }
        for t in [Tool::Hand, Tool::Zoom, Tool::Brush, Tool::Puppet, Tool::RotoBrush, Tool::Orbit] {
            assert!(!hosted(t), "{t:?}");
        }
    }

    /// The tools' keys run in a host (W, Y, Q, G, Cmd+T; V back to the Selection tool), cycling
    /// a slot as EffectCraft's Tools panel does.
    #[test]
    fn tool_keys_pick_the_tools_in_a_host() {
        let (mut h, _, _) = mini();
        let ctx = ctx();
        let key = |h: &mut Mini, key: egui::Key, modifiers: egui::Modifiers| {
            let input = egui::RawInput { events: vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }], ..Default::default() };
            let _ = ctx.run_ui(input, |ui| {
                crate::panels::timeline::keys::forward_shortcuts(h, ui.ctx(), crate::dock::PanelKind::Timeline);
            });
            h.ui.tool
        };
        assert_eq!(key(&mut h, egui::Key::W, egui::Modifiers::NONE), Tool::Rotate);
        assert_eq!(key(&mut h, egui::Key::Y, egui::Modifiers::NONE), Tool::PanBehind);
        assert_eq!(key(&mut h, egui::Key::Q, egui::Modifiers::NONE), Tool::Rectangle);
        assert_eq!(key(&mut h, egui::Key::Q, egui::Modifiers::NONE), Tool::RoundedRect, "Q again: the slot's next tool");
        assert_eq!(key(&mut h, egui::Key::G, egui::Modifiers::NONE), Tool::Pen);
        assert_eq!(key(&mut h, egui::Key::T, egui::Modifiers::COMMAND), Tool::Type);
        assert_eq!(key(&mut h, egui::Key::V, egui::Modifiers::NONE), Tool::Selection);
        // Tools a card hasn't are refused by name, never panicking.
        assert!(h.ui.choose_tool("tool.nothing").is_err());
    }

    #[test]
    fn the_rotation_tool_turns_the_layer_under_the_pointer() {
        let (mut h, _, bx) = mini();
        h.ui.tool = Tool::Rotate;
        let ctx = ctx();
        // About the Box's anchor (160, 90): from straight right to straight down is +90°.
        drag(&ctx, &mut h, &[pos2(175.0, 90.0), pos2(185.0, 90.0), pos2(160.0, 115.0)]);
        assert_eq!(h.session.state.selected_layers, vec![bx], "the press selected the Box");
        let r = f(&h, bx, "transform/rotation");
        assert!((r - 90.0).abs() < 0.5, "rotation {r}");
        // One drag, one undo step.
        h.session.execute("edit.undo", json!({})).unwrap();
        assert!(f(&h, bx, "transform/rotation").abs() < 1e-6);
    }

    #[test]
    fn pan_behind_moves_the_anchor_and_keeps_the_layer_in_place() {
        let (mut h, _, bx) = mini();
        h.ui.tool = Tool::PanBehind;
        let ctx = ctx();
        drag(&ctx, &mut h, &[pos2(160.0, 90.0), pos2(170.0, 90.0), pos2(175.0, 95.0)]);
        let a = v3(&h, bx, "transform/anchor");
        let p = v3(&h, bx, "transform/position");
        assert!((a[0] - 35.0).abs() < 0.5 && (a[1] - 25.0).abs() < 0.5, "anchor {a:?}");
        assert!((p[0] - 175.0).abs() < 0.5 && (p[1] - 95.0).abs() < 0.5, "position {p:?}");
    }

    #[test]
    fn a_shape_tool_drag_draws_a_mask_on_the_selected_layer() {
        let (mut h, plate, _) = mini();
        h.session.execute("layer.select", json!({"layers": [plate.0]})).unwrap();
        h.ui.tool = Tool::Ellipse;
        let ctx = ctx();
        assert_eq!(masks(&h, plate), 0);
        drag(&ctx, &mut h, &[pos2(20.0, 20.0), pos2(40.0, 40.0), pos2(90.0, 60.0)]);
        assert_eq!(masks(&h, plate), 1, "a mask on the Plate");
        // Without a selection the tool makes a shape layer instead.
        h.session.execute("edit.deselectAll", json!({})).unwrap();
        h.ui.tool = Tool::Rectangle;
        let n = h.session.active_comp().unwrap().layers.len();
        drag(&ctx, &mut h, &[pos2(200.0, 20.0), pos2(220.0, 40.0), pos2(260.0, 70.0)]);
        let c = h.session.active_comp().unwrap();
        assert_eq!(c.layers.len(), n + 1);
        assert!(c.layers.iter().any(|l| matches!(l.source, LayerSource::Shape)));
    }

    #[test]
    fn the_pen_draws_a_closed_mask_and_its_vertices_drag() {
        let (mut h, plate, _) = mini();
        h.session.execute("layer.select", json!({"layers": [plate.0]})).unwrap();
        h.ui.tool = Tool::Pen;
        let ctx = ctx();
        for p in [pos2(30.0, 30.0), pos2(120.0, 30.0), pos2(80.0, 120.0), pos2(30.0, 30.0)] {
            click(&ctx, &mut h, p);
        }
        assert_eq!(masks(&h, plate), 1);
        let path = |h: &Mini| {
            let c = h.session.active_comp().unwrap();
            let l = c.layer(plate).unwrap();
            let g = l.masks().unwrap().groups().next().unwrap();
            let v = l.props.find_group(g.uid).unwrap().get("path").unwrap().value_at(l.layer_time(h.session.time()));
            v.as_path().cloned().unwrap()
        };
        let sp = path(&h);
        assert_eq!(sp.vertices.len(), 3);
        assert!(sp.closed, "clicking the first vertex closed it");
        // V: drag the second vertex 10 px right.
        h.ui.tool = Tool::Selection;
        frame(&ctx, &mut h, vec![]);
        drag(&ctx, &mut h, &[pos2(120.0, 30.0), pos2(126.0, 30.0), pos2(130.0, 30.0)]);
        let moved = path(&h).vertices[1];
        assert!((moved[0] - 130.0).abs() < 0.5 && (moved[1] - 30.0).abs() < 0.5, "vertex {moved:?}");
    }

    #[test]
    fn the_type_tool_creates_text_and_types_in_place() {
        let (mut h, _, _) = mini();
        h.ui.tool = Tool::Type;
        let ctx = ctx();
        click(&ctx, &mut h, pos2(40.0, 60.0));
        let editing = h.session.state.text_edit.clone().expect("a text edit began");
        frame(&ctx, &mut h, vec![Event::Text("Hi".into())]);
        frame(&ctx, &mut h, vec![]);
        let doc = effectcraft_engine::commands::text_edit::layer_doc(&h.session, editing.layer).unwrap();
        assert_eq!(doc.text, "Hi");
        // Escape commits.
        frame(&ctx, &mut h, vec![Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }]);
        assert!(h.session.state.text_edit.is_none());
    }

    // ---- EMBED14: guides, grids, rulers, motion paths, Mask Feather ----

    /// The View toggles run in a host, and the picture gets nothing over it until one is on.
    #[test]
    fn guides_and_grids_toggle_and_draw_only_when_on() {
        let (mut h, _, _) = mini();
        let ctx = ctx();
        h.session.execute("view.addGuide", json!({"orientation": "vertical", "position": 100})).unwrap();
        h.ui.viewer.guides = false;
        let baseline = painted(&ctx, &mut h, false);
        assert_eq!(painted(&ctx, &mut h, true), baseline, "nothing over the picture while everything is off");
        for id in ["view.grid", "view.safeMargins", "view.guides", "view.rulers", "view.snapToGrid", "view.snapToGuides", "view.lockGuides"] {
            let r = th::run(&mut h, &ctx, id, &json!({})).expect("a host runs it");
            assert_eq!(r.unwrap()["value"], json!(true), "{id} turned on");
        }
        assert!(h.ui.viewer.grid && h.ui.viewer.safe_margins && h.ui.viewer.guides && h.ui.viewer.rulers);
        assert!(painted(&ctx, &mut h, true) > baseline, "the grid, safe areas and guide draw");
        // `value` sets rather than toggles; each is off again.
        for id in ["view.grid", "view.safeMargins", "view.guides", "view.rulers"] {
            th::run(&mut h, &ctx, id, &json!({"value": false})).unwrap().unwrap();
        }
        assert_eq!(painted(&ctx, &mut h, true), baseline);
        // The grid and guide options menu: the same entries as the viewer's, toggled by index.
        let items = grid_options(&h);
        assert_eq!(
            items.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(),
            ["Title/Action Safe", "Proportional Grid", "Grid", "Guides", "Rulers", "-", "3D Reference Axes"]
        );
        assert!(!grid_options_on(&h));
        toggle_grid_option(&mut h, 1);
        assert!(h.ui.viewer.proportional_grid && grid_options_on(&h));
        assert!(painted(&ctx, &mut h, true) > baseline);
        toggle_grid_option(&mut h, 1);
        for junk in [5, 7, usize::MAX] {
            toggle_grid_option(&mut h, junk);
        }
        assert!(!grid_options_on(&h));
    }

    /// A guide drags with the Selection tool (one undo step) and leaves when dragged off; one
    /// dragged out of the top ruler is a horizontal guide where it is dropped.
    #[test]
    fn guides_drag_and_come_out_of_the_rulers() {
        let (mut h, _, _) = mini();
        let ctx = ctx();
        h.session.execute("view.addGuide", json!({"orientation": "vertical", "position": 100})).unwrap();
        h.ui.viewer.guides = true;
        let guide = |h: &Mini| {
            let c = h.session.active_comp().unwrap();
            c.guides.first().map(|g| g.px(c.width, c.height))
        };
        drag(&ctx, &mut h, &[pos2(100.0, 50.0), pos2(108.0, 50.0), pos2(120.0, 50.0)]);
        assert!((guide(&h).unwrap() - 120.0).abs() < 0.5, "{:?}", guide(&h));
        h.session.execute("edit.undo", json!({})).unwrap();
        assert!((guide(&h).unwrap() - 100.0).abs() < 0.5, "one undo step");
        drag(&ctx, &mut h, &[pos2(100.0, 50.0), pos2(150.0, 50.0), pos2(395.0, 50.0)]);
        assert!(guide(&h).is_none(), "dragged off the picture: removed");
        // Rulers outside a picture at 50,40: drag from the top ruler down into the picture.
        h.ui.viewer.rulers = true;
        let m = map_for(Rect::from_min_size(pos2(50.0, 40.0), vec2(320.0, 180.0)), [320, 180]).unwrap();
        drag_on(&ctx, &mut h, &[pos2(150.0, 32.0), pos2(150.0, 60.0), pos2(150.0, 100.0)], m);
        let c = h.session.active_comp().unwrap();
        let g = c.guides.first().expect("a guide from the ruler");
        assert!(!g.vertical && (g.px(c.width, c.height) - 60.0).abs() < 0.5, "{g:?}");
    }

    /// The selected layer's motion path: a key drags (the value moves, one undo step), at any
    /// canvas zoom.
    #[test]
    fn a_motion_path_key_drags_as_one_undo_step() {
        let (mut h, _, bx) = mini();
        keyed_box(&mut h, bx);
        let ctx = ctx();
        frame(&ctx, &mut h, vec![]);
        drag(&ctx, &mut h, &[pos2(160.0, 90.0), pos2(165.0, 95.0), pos2(170.0, 100.0)]);
        let v = position_keys(&h, bx)[0].value.as_vec3();
        assert!((v[0] - 170.0).abs() < 0.5 && (v[1] - 100.0).abs() < 0.5, "key {v:?}");
        h.session.execute("edit.undo", json!({})).unwrap();
        let v = position_keys(&h, bx)[0].value.as_vec3();
        assert!((v[0] - 160.0).abs() < 1e-6 && (v[1] - 90.0).abs() < 1e-6, "one undo step: {v:?}");
        // The card at 200 % at 50,40 on screen: the key under the pointer is the same one.
        let m = map_for(Rect::from_min_size(pos2(50.0, 40.0), vec2(640.0, 360.0)), [320, 180]).unwrap();
        drag_on(&ctx, &mut h, &[pos2(370.0, 220.0), pos2(380.0, 230.0), pos2(390.0, 240.0)], m);
        let v = position_keys(&h, bx)[0].value.as_vec3();
        assert!((v[0] - 170.0).abs() < 0.5 && (v[1] - 100.0).abs() < 0.5, "at 200 %: {v:?}");
        // The Motion Path setting None shows (and takes) nothing.
        h.session.prefs.composition.motion_path = "none".into();
        let before = position_keys(&h, bx)[0].value.as_vec3();
        drag(&ctx, &mut h, &[pos2(170.0, 100.0), pos2(180.0, 110.0), pos2(190.0, 120.0)]);
        assert_eq!(position_keys(&h, bx)[0].value.as_vec3(), before);
    }

    /// A key's spatial tangent handle drags the tangent (one undo step).
    #[test]
    fn a_motion_path_tangent_drags_as_one_undo_step() {
        let (mut h, _, bx) = mini();
        keyed_box(&mut h, bx);
        let ctx = ctx();
        let keys = position_keys(&h, bx);
        let (_, out) = if keys[0].spatial_auto {
            effectcraft_engine::keyframe::spatial_handles(&keys, 0)
        } else {
            effectcraft_engine::keyframe::spatial_tangents(&keys, 0)
        };
        assert!(out[0].abs() > 1.0, "the first key shows an out handle: {out:?}");
        let at = pos2(160.0 + out[0] as f32, 90.0 + out[1] as f32);
        frame(&ctx, &mut h, vec![]);
        drag(&ctx, &mut h, &[at, at + vec2(0.0, 10.0), at + vec2(0.0, 30.0)]);
        let k = &position_keys(&h, bx)[0];
        assert!((k.spatial_out[1] - 30.0).abs() < 0.5, "out tangent {:?}", k.spatial_out);
        h.session.execute("edit.undo", json!({})).unwrap();
        assert_eq!(position_keys(&h, bx)[0].spatial_out, keys[0].spatial_out, "one undo step");
    }

    /// The Mask Feather tool (G cycles the Pen slot to it): a drag from the mask's edge adds a
    /// feather point and pulls its radius out; its handle drags the radius (one undo step each).
    #[test]
    fn the_mask_feather_tool_adds_and_drags_feather_points() {
        let (mut h, plate, _) = mini();
        h.session.execute("mask.new", json!({"layer": plate.0, "vertices": [[40, 40], [120, 40], [120, 120], [40, 120]], "closed": true})).unwrap();
        h.session.execute("layer.select", json!({"layers": [plate.0]})).unwrap();
        let ctx = ctx();
        for _ in 0..5 {
            h.ui.choose_tool("tool.pen").unwrap();
        }
        assert_eq!(h.ui.tool, Tool::MaskFeather, "the Pen slot's fifth tool");
        assert!(hosted(Tool::MaskFeather) && active(&h) == Tool::MaskFeather);
        drag(&ctx, &mut h, &[pos2(80.0, 40.0), pos2(80.0, 35.0), pos2(80.0, 30.0)]);
        let f = feather_points(&h, plate);
        assert_eq!(f.len(), 1, "a feather point");
        assert!((f[0].radius - 10.0).abs() < 0.5, "outer radius {}", f[0].radius);
        // Its handle (80,30) drags the radius out to 20.
        drag(&ctx, &mut h, &[pos2(80.0, 30.0), pos2(80.0, 25.0), pos2(80.0, 20.0)]);
        let f = feather_points(&h, plate);
        assert_eq!(f.len(), 1);
        assert!((f[0].radius - 20.0).abs() < 0.5, "radius {}", f[0].radius);
        h.session.execute("edit.undo", json!({})).unwrap();
        assert!((feather_points(&h, plate)[0].radius - 10.0).abs() < 0.5, "one undo step");
        // A click (no drag) changes nothing.
        click(&ctx, &mut h, pos2(80.0, 120.0));
        assert_eq!(feather_points(&h, plate).len(), 1);
    }

    /// The feather radius along a path between feather points, and the boundary drawn from it.
    #[test]
    fn the_feather_boundary_follows_the_feather_points() {
        use effectcraft_engine::keyframe::{FeatherPoint, ShapePath};
        let mut sp = ShapePath::polygon(&[[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]], true);
        assert!(ov::feather_radius_at(&sp, 0.5).is_none());
        sp.feather = vec![FeatherPoint { segment: 0, t: 0.5, radius: 10.0, tension: 1.0 }];
        assert_eq!(ov::feather_radius_at(&sp, 2.3), Some(10.0), "one point: everywhere");
        sp.feather.push(FeatherPoint { segment: 2, t: 0.5, radius: 30.0, tension: 1.0 });
        let mid = ov::feather_radius_at(&sp, 1.5).unwrap();
        assert!((mid - 20.0).abs() < 1e-9, "linear halfway: {mid}");
        let wrap = ov::feather_radius_at(&sp, 3.5).unwrap();
        assert!((wrap - 20.0).abs() < 1e-9, "a closed path wraps: {wrap}");
        for junk in [f64::NAN, f64::INFINITY, -1e300, 1e300] {
            let _ = ov::feather_radius_at(&sp, junk);
        }
        let p = ov::PathInfo { layer: LayerId(1), uid: 2, m: effectcraft_engine::geom::Mat3::IDENTITY, sp, is_mask: true, feather: vec![] };
        let b = ov::feather_boundary(&p, &map());
        assert!(!b.is_empty());
        // Halfway along the top edge the boundary is 10 px above it (outside).
        let top = b.iter().find(|s| s.0 == 0 && (s.1 - 0.5).abs() < 1e-9).unwrap();
        assert!((top.2.y + 10.0).abs() < 0.01 && (top.2.x - 50.0).abs() < 0.01, "{top:?}");
    }

    /// Junk maps and pointer positions never panic and do nothing.
    #[test]
    fn junk_never_panics() {
        let (mut h, _, bx) = mini();
        let ctx = ctx();
        keyed_box(&mut h, bx);
        h.ui.viewer.grid = true;
        h.ui.viewer.rulers = true;
        h.ui.viewer.guides = true;
        h.ui.viewer.safe_margins = true;
        h.session.prefs.grids.grid_spacing = 0.0;
        let _ = h.session.execute("view.addGuide", json!({"orientation": "horizontal", "position": 1e12}));
        for tool in [Tool::Rotate, Tool::PanBehind, Tool::Star, Tool::Pen, Tool::Type, Tool::Selection, Tool::PenDelete, Tool::PenAdd, Tool::MaskFeather] {
            h.ui.tool = tool;
            for m in [
                ViewerMap { origin: Pos2::new(f32::NAN, 0.0), zoom: 1.0, comp: [320.0, 180.0], area: Rect::from_min_size(Pos2::ZERO, vec2(320.0, 180.0)) },
                ViewerMap { origin: Pos2::ZERO, zoom: 0.0, comp: [320.0, 180.0], area: Rect::from_min_size(Pos2::ZERO, vec2(320.0, 180.0)) },
                ViewerMap { origin: Pos2::ZERO, zoom: f32::INFINITY, comp: [0.0, 0.0], area: Rect::NOTHING },
                ViewerMap { origin: Pos2::ZERO, zoom: 1e-30, comp: [320.0, 180.0], area: Rect::from_min_size(Pos2::ZERO, vec2(1e-3, 1e-3)) },
            ] {
                for at in [pos2(f32::NAN, 1.0), pos2(1e30, -1e30), pos2(5.0, 5.0)] {
                    let input = egui::RawInput { events: vec![Event::PointerMoved(at), button(at, true), button(at, false)], ..Default::default() };
                    let _ = ctx.run_ui(input, |ui| {
                        let _ = tools(&mut h, ui, m, Options { id: egui::Id::new("junk"), space_pan: false });
                        view_overlays(&mut h, ui, m);
                        let _ = edit_text_at(&mut h, ui.ctx(), m, at);
                    });
                }
            }
        }
    }
}
