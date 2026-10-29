//! EMBED16: the Tools bar's tool slots, for EffectCraft's own header and for a host that shows a
//! comp somewhere else (FigCraft hosts the Timeline under a card on its canvas and puts this row
//! in its header).
//!
//! [`slots`] is the one drawing of the tool slots: the same tools, order, group separators,
//! flyouts (right-click or a long press), tooltips with the keymap's keys, icons (the owner-local
//! artwork through [`crate::icons::paint`] when present, else EffectCraft's own) and the
//! Ctrl+double-click on Pan Behind (Center Anchor Point in Layer Content). EffectCraft's header
//! draws it at 26 points; a host passes its own size, and the row works in a scaled layer (a
//! canvas) because every popup it opens is anchored in window coordinates by egui.
//!
//! A host whose comp can't take some tools passes [`Options::works`]: those slots are drawn
//! dimmed, their tooltip says where they work, and picking one leaves the tool as it is (the
//! status line says why), so the host's view never sits in a tool it can't run.

use egui::{Color32, Rect, Stroke, pos2, vec2};
use serde_json::json;

use super::timeline::TimelineHost;
use crate::state::Tool;
use crate::widgets;

/// The header's slot size (points): the row's other sizes scale from it.
pub const SIDE: f32 = 26.0;

/// How a host draws [`slots`].
pub struct Options<'a> {
    /// The slot's side (points in the row's layer); EffectCraft's header uses [`SIDE`].
    pub side: f32,
    /// Salts the row's ids, so a host's row never shares state with EffectCraft's own (`None`:
    /// EffectCraft's header, plain ids).
    pub id_scope: Option<egui::Id>,
    /// Whether the host's comp can use a tool (`None`: every tool works).
    pub works: Option<&'a dyn Fn(Tool) -> bool>,
    /// The tooltip line added to a tool that doesn't work there.
    pub elsewhere: &'a str,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self { side: SIDE, id_scope: None, works: None, elsewhere: "" }
    }
}

impl Options<'_> {
    fn id(&self, salt: impl std::hash::Hash + std::fmt::Debug) -> egui::Id {
        match self.id_scope {
            Some(scope) => scope.with(salt),
            None => egui::Id::new(salt),
        }
    }
    fn works(&self, tool: Tool) -> bool {
        self.works.is_none_or(|w| w(tool))
    }
}

/// Whether a slot is followed by a group separator (after the view tools, the camera tools, Pan
/// Behind, Type and the paint tools).
pub fn separator_after(slot: usize) -> bool {
    matches!(slot, 2 | 5 | 7 | 10 | 13)
}

/// The row's width for slot side `side` (points): what [`slots`] returns minus where it started.
pub fn width(side: f32) -> f32 {
    let k = side / SIDE;
    let seps = (0..Tool::SLOTS.len()).filter(|s| separator_after(*s)).count() as f32;
    Tool::SLOTS.len() as f32 * 28.0 * k + seps * 10.0 * k
}

/// The tool slots from `x` (left edge) centred on `cy`, in the row's layer. Returns the right
/// edge. A click picks the slot's tool, a right-click or a long press opens its flyout.
pub fn slots(host: &mut dyn TimelineHost, ui: &mut egui::Ui, mut x: f32, cy: f32, opts: &Options) -> f32 {
    let t = host.tokens();
    let p = ui.painter().clone();
    let side = if opts.side.is_finite() && opts.side > 0.0 { opts.side } else { SIDE };
    let k = side / SIDE;
    let held_id = opts.id("tool-slot-held");
    if ui.input(|i| i.pointer.any_pressed()) {
        ui.data_mut(|d| d.remove::<egui::Id>(held_id));
    }
    for (si, slot) in Tool::SLOTS.iter().enumerate() {
        let Some(first) = slot.first().copied() else { continue };
        let cur = host.ui().slot_tools.get(si).copied().unwrap_or(first);
        let r = Rect::from_min_size(pos2(x, cy - side / 2.0), vec2(side, side));
        let tool = host.ui().tool;
        let active = slot.contains(&tool);
        let works = opts.works(cur);
        let id = opts.id(("tool-slot", si));
        let resp = widgets::icon_button(ui, r, cur.icon(), active, &t, id);
        if !works && !active {
            // Dimmed: a veil of the bar's colour over the glyph.
            p.rect_filled(r, 4.0 * k, t.header_bg.gamma_multiply(0.6));
        }
        if slot.len() > 1 {
            // Flyout corner triangle.
            p.add(egui::Shape::convex_polygon(
                vec![pos2(r.max.x - 2.0 * k, r.max.y - 6.0 * k), pos2(r.max.x - 2.0 * k, r.max.y - 2.0 * k), pos2(r.max.x - 6.0 * k, r.max.y - 2.0 * k)],
                if active { Color32::WHITE } else { t.text_dim },
                Stroke::NONE,
            ));
        }
        host.auto_mut().add_response(&format!("tools.{cur:?}"), &resp, cur.label());
        let mut tip = match cur.shortcut() {
            Some(s) => format!("{} ({})", cur.label(), crate::menus::shortcut_text(s)),
            None => cur.label().to_string(),
        };
        if !works && !opts.elsewhere.is_empty() {
            tip = format!("{tip}\n{}", opts.elsewhere);
        }
        let resp = resp.on_hover_text(tip);
        let held = ui.data(|d| d.get_temp::<egui::Id>(held_id)) == Some(id);
        if resp.clicked() && !held {
            pick(host, cur, opts);
        }
        // Ctrl+double-click Pan Behind: Center Anchor Point in Layer Content.
        if cur == Tool::PanBehind
            && resp.double_clicked()
            && ui.input(|i| i.modifiers.command)
            && let Err(e) = host.invoke(ui.ctx(), "layer.centerAnchor", json!({}))
        {
            host.ui_mut().status = e;
        }
        if slot.len() > 1 {
            let held_for = resp.is_pointer_button_down_on().then(|| ui.input(|i| i.pointer.press_start_time().map(|s| i.time - s))).flatten();
            if let Some(seconds) = held_for
                && seconds < 0.35
            {
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(0.35 - seconds));
            }
            let long_press = held_for.is_some_and(|seconds| seconds >= 0.35) && !held;
            let command = if resp.secondary_clicked() || long_press {
                if long_press {
                    ui.data_mut(|d| d.insert_temp(held_id, id));
                }
                Some(egui::SetOpenCommand::Bool(true))
            } else if resp.clicked() && !held {
                Some(egui::SetOpenCommand::Bool(false))
            } else {
                None
            };
            let close_behavior = if held && resp.clicked() { egui::PopupCloseBehavior::IgnoreClicks } else { egui::PopupCloseBehavior::CloseOnClickOutside };
            egui::Popup::menu(&resp).style(crate::widgets::menu_style).open_memory(command).close_behavior(close_behavior).show(|ui| {
                for tool in slot.iter() {
                    let row = ui.selectable_label(host.ui().tool == *tool, tool.label());
                    host.auto_mut().add(&format!("tools.select.{tool:?}"), row.rect, tool.label());
                    if row.clicked() {
                        if pick(host, *tool, opts)
                            && let Some(saved) = host.ui_mut().slot_tools.get_mut(si)
                        {
                            *saved = *tool;
                        }
                        ui.close();
                    }
                }
            });
        }
        x += 28.0 * k;
        if separator_after(si) {
            x += 4.0 * k;
            p.line_segment([pos2(x, cy - 10.0 * k), pos2(x, cy + 10.0 * k)], Stroke::new(1.0, t.separator));
            x += 6.0 * k;
        }
    }
    x
}

/// Pick `tool` when the host's comp can use it; else say where it works. Returns whether it
/// was picked.
fn pick(host: &mut dyn TimelineHost, tool: Tool, opts: &Options) -> bool {
    if opts.works(tool) {
        host.ui_mut().tool = tool;
        return true;
    }
    host.ui_mut().status =
        if opts.elsewhere.is_empty() { format!("{} isn't available here", tool.label()) } else { format!("{}: {}", tool.label(), opts.elsewhere) };
    false
}
