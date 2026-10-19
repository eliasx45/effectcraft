//! Panel bodies.

// Every `src/panels/*.rs` file is a module, declared by build.rs (docs/architecture.md,
// "Self-registering modules"); declare one here only for another visibility or a submodule folder.
include!(concat!(env!("OUT_DIR"), "/panels.rs"));
pub mod timeline;

use effectcraft_engine::Session;
use effectcraft_engine::project::Comp;
use effectcraft_engine::time::Tick;
use egui::Rect;

use crate::EffectcraftApp;
use crate::dock::PanelKind;

/// Drag-and-drop payloads between panels.
#[derive(Clone, Debug)]
pub enum DragPayload {
    /// A project item (footage, comp, solid).
    Item(u64),
    /// An effect id from Effects & Presets.
    Effect(String),
    /// A property dragged from the timeline (to the Essential Graphics panel).
    Property { layer: u64, prop: u64 },
    /// Files from the Media Browser (import; into the timeline: import and add).
    Files(Vec<String>),
    /// A Motion Library card id (applied where it is dropped).
    LibraryCard(String),
}

/// Layers carried by a Project item drag. Dragging a selected item uses the
/// complete selection, while dragging an unselected item uses only that item.
/// Folders and data files are not composition layers.
pub(crate) fn project_drop_items(app: &EffectcraftApp, dragged: u64) -> Vec<u64> {
    use effectcraft_engine::project::{FootageKind, ItemId, ItemKind};

    let selected = &app.session.state.project_selection;
    let candidates: Vec<u64> = if selected.iter().any(|id| id.0 == dragged) {
        selected.iter().map(|id| id.0).collect()
    } else {
        // One unselected item goes through as is, so a folder or data file still gets the
        // engine's error instead of a silent no-op.
        return vec![dragged];
    };
    candidates
        .into_iter()
        .filter(|id| {
            app.session.project.item(ItemId(*id)).is_some_and(|item| match &item.kind {
                ItemKind::Comp(_) | ItemKind::Solid(_) => true,
                ItemKind::Footage(f) => f.kind != FootageKind::Data,
                ItemKind::Folder => false,
            })
        })
        .collect()
}

/// The current time formatted per project settings (timecode with `;` for drop-frame, frames
/// or Feet + Frames).
/// Layer `id` of `comp`, shared (a reference count, not a copy of its property tree): panels
/// that keep the layer while they mutate the app hold this.
pub(crate) fn layer_arc(comp: &Comp, id: effectcraft_engine::project::LayerId) -> Option<std::sync::Arc<effectcraft_engine::project::Layer>> {
    comp.layers.arcs().iter().find(|l| l.id == id).cloned()
}

pub fn timecode(session: &Session, comp: &Comp, t: Tick) -> String {
    effectcraft_engine::commands::time::display_time(session, comp, t)
}

/// Ctrl/Cmd-click on a current-time display toggles the project's Time Display Style between
/// Timecode and Frames, as in After Effects (through Project Settings: one undo step).
pub(crate) fn toggle_time_display(app: &mut EffectcraftApp, resp: &egui::Response) {
    if !(resp.clicked() && resp.ctx.input(|i| i.modifiers.command)) {
        return;
    }
    let timecode = app.session.project.settings.time_display == effectcraft_engine::project::TimeDisplayStyle::Timecode;
    if let Err(e) = app.session.execute("file.projectSettings", serde_json::json!({"timeDisplay": if timecode { "frames" } else { "timecode" }})) {
        app.ui.status = e.to_string();
    }
}

/// Spacebar held outside a text field: the Hand tool (drags pan the viewer and scroll the
/// Timeline).
pub(crate) fn space_hand(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_down(egui::Key::Space)) && !ctx.egui_wants_keyboard_input()
}

/// What a scroll gesture over a viewer (Composition, Layer) does this frame.
pub(crate) struct ViewerScroll {
    /// Points to pan by: a two-finger trackpad scroll.
    pub pan: egui::Vec2,
    /// Factor to zoom by around the pointer: a pinch, Ctrl/Cmd+scroll, a mouse wheel, or
    /// Alt + a trackpad scroll.
    pub zoom: f32,
}

/// Splits the frame's scroll input into a viewer pan and a zoom: a mouse wheel zooms and a
/// trackpad's two-finger scroll pans.
///
/// A wheel scrolls in lines, but in a browser and with some high-resolution wheels it scrolls in
/// points like a trackpad does, so the unit alone doesn't tell them apart. A scroll in points is
/// a trackpad's once it has shown what a wheel can't do: a touch phase (macOS reports where the
/// fingers land), movement on both axes at once, or a pinch. That holds until a wheel scrolls in
/// lines. Until then the vertical part zooms; the horizontal part always pans.
pub(crate) fn viewer_scroll(ctx: &egui::Context) -> ViewerScroll {
    let id = egui::Id::new("viewer-scroll-trackpad");
    // The scroll is smoothed over the frames after its event, so the device is remembered.
    let was: bool = ctx.data(|d| d.get_temp(id)).unwrap_or(false);
    let trackpad = ctx.input(|i| {
        i.events.iter().fold(was, |t, e| match e {
            egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta, phase, modifiers, .. } => {
                // Shift turns a wheel's scroll sideways, which is still one axis.
                t || *phase == egui::TouchPhase::Start || (delta.x != 0.0 && delta.y != 0.0 && !modifiers.shift)
            }
            egui::Event::MouseWheel { .. } => false,
            egui::Event::Zoom(_) => true,
            _ => t,
        })
    });
    if trackpad != was {
        ctx.data_mut(|d| d.insert_temp(id, trackpad));
    }
    let (scroll, pinch, alt) = ctx.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.modifiers.alt));
    let pinch = if pinch.is_finite() && pinch > 0.0 { pinch } else { 1.0 };
    let scroll = if scroll.x.is_finite() && scroll.y.is_finite() { scroll } else { egui::Vec2::ZERO };
    if trackpad && !alt {
        ViewerScroll { pan: scroll, zoom: pinch }
    } else if alt {
        ViewerScroll { pan: egui::Vec2::ZERO, zoom: pinch * (scroll.y / 300.0).exp() }
    } else {
        ViewerScroll { pan: egui::vec2(scroll.x, 0.0), zoom: pinch * (scroll.y / 300.0).exp() }
    }
}

pub fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui, p: PanelKind, rect: Rect) {
    // Restore the parent cursor before another panel processes the click, never afterward.
    // Menu clicks are outside panel bodies, so Edit/Animation retain inline source targeting.
    if p != PanelKind::Timeline
        && !crate::widgets::any_menu_open(ui.ctx())
        && !egui::Popup::is_any_open(ui.ctx())
        && ui.rect_contains_pointer(rect)
        && ui.input(|i| i.pointer.any_pressed())
    {
        timeline::clear_inline_focus(app);
    }
    match p {
        PanelKind::Composition => viewers::show(app, ui, 0, rect),
        PanelKind::Viewer(n) => viewers::show(app, ui, n, rect),
        PanelKind::Timeline => timeline::show(app, ui, rect),
        PanelKind::Project => project::show(app, ui, rect),
        PanelKind::EffectControls => effect_controls::show(app, ui, rect),
        PanelKind::EffectsPresets => effects_presets::show(app, ui, rect),
        PanelKind::Info => info::show(app, ui, rect),
        PanelKind::Preview => misc::preview(app, ui, rect),
        PanelKind::Character => text_panels::character(app, ui, rect),
        PanelKind::Paragraph => text_panels::paragraph(app, ui, rect),
        PanelKind::Align => text_panels::align(app, ui, rect),
        PanelKind::Properties => properties::show(app, ui, rect),
        PanelKind::Audio => misc::audio(app, ui, rect),
        PanelKind::History => misc::history(app, ui, rect),
        PanelKind::Markers => misc::markers(app, ui, rect),
        PanelKind::MaskInterpolation => anim_tools::mask_interpolation(app, ui, rect),
        PanelKind::ScriptConsole => script_console::show(app, ui, rect),
        PanelKind::Wiggler => anim_tools::wiggler(app, ui, rect),
        PanelKind::Smoother => anim_tools::smoother(app, ui, rect),
        PanelKind::MotionSketch => anim_tools::motion_sketch(app, ui, rect),
        PanelKind::RenderQueue => render_queue::show(app, ui, rect),
        PanelKind::Tracker => tracker::show(app, ui, rect),
        PanelKind::Layer => layer_panel::show(app, ui, rect),
        PanelKind::Paint => paint_panels::paint(app, ui, rect),
        PanelKind::Brushes => paint_panels::brushes(app, ui, rect),
        PanelKind::EssentialGraphics => essential::show(app, ui, rect),
        PanelKind::ScriptPanel(id) => scriptui_view::panel(app, ui, rect, id),
        PanelKind::Flowchart => flowchart::show(app, ui, rect),
        PanelKind::LumetriScopes => scopes_panel::show(app, ui, rect),
        PanelKind::Footage => footage_panel::show(app, ui, rect),
        PanelKind::MediaBrowser => media_panels::media_browser(app, ui, rect),
        PanelKind::Metadata => media_panels::metadata(app, ui, rect),
        PanelKind::Progress => media_panels::progress(app, ui, rect),
        PanelKind::SmartFreeze => smart_freeze::show(app, ui, rect),
        PanelKind::ContentAwareFill => content_fill_panel::show(app, ui, rect),
        PanelKind::CreateNullsFromPaths => path_vr_panels::create_nulls(app, ui, rect),
        PanelKind::VrCompEditor => path_vr_panels::vr_editor(app, ui, rect),
        PanelKind::MotionLibrary => motion_library::show(app, ui, rect),
        PanelKind::Script => script_panel::show(app, ui, rect),
    }
}

/// The panel menu in a panel's own native window (Move Panel to New Window): the fork's
/// [`panel_menu`] draws a menu opened for that window there (see [`panel_menu::open_in`]).
pub fn panel_menu_popup(app: &mut EffectcraftApp, ui: &mut egui::Ui) {
    panel_menu::show(app, ui);
}
