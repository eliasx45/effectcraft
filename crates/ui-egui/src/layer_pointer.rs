//! Pointer positions in the coordinates a panel draws in.
//!
//! egui keeps the pointer in window coordinates. A host may draw a panel in a layer that egui
//! transforms (`Context::set_transform_layer`: FigCraft draws the Timeline scaled on its canvas,
//! under the card it edits). egui's own responses come back in the layer's coordinates, but a raw
//! read (`i.pointer.hover_pos()`, `press_origin()`, `delta()`…) does not: the panels pass those
//! through [`Lp`]. In EffectCraft's own window no layer is transformed and [`Lp`] changes nothing.

use egui::emath::TSTransform;
use egui::{Context, Id, LayerId, Pos2, Rect, Ui, Vec2};

/// From window coordinates into one layer's (`None`: the layer isn't transformed).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lp(Option<TSTransform>);

fn panel_layer_id() -> Id {
    Id::new("effectcraft-panel-layer")
}

impl Lp {
    /// For what `ui` draws.
    pub fn of(ui: &Ui) -> Lp {
        Lp::layer(ui.ctx(), ui.layer_id())
    }

    /// For layer `layer`.
    pub fn layer(ctx: &Context, layer: LayerId) -> Lp {
        Lp(ctx.layer_transform_from_global(layer))
    }

    /// Remember `ui`'s layer as the panel's (helpers that only get the context use it this pass).
    pub fn mark(ui: &Ui) {
        let layer = ui.layer_id();
        ui.ctx().data_mut(|d| d.insert_temp(panel_layer_id(), layer));
    }

    /// For the layer the panel being drawn marked ([`Lp::mark`]); untransformed when none did.
    pub fn panel(ctx: &Context) -> Lp {
        match ctx.data(|d| d.get_temp::<LayerId>(panel_layer_id())) {
            Some(layer) => Lp::layer(ctx, layer),
            None => Lp(None),
        }
    }

    /// A window point in the layer's coordinates.
    pub fn p(self, p: Pos2) -> Pos2 {
        match self.0 {
            Some(t) => t * p,
            None => p,
        }
    }

    pub fn pos(self, p: Option<Pos2>) -> Option<Pos2> {
        p.map(|p| self.p(p))
    }

    pub fn pair(self, (a, b): (Option<Pos2>, Option<Pos2>)) -> (Option<Pos2>, Option<Pos2>) {
        (self.pos(a), self.pos(b))
    }

    /// A window movement in the layer's coordinates.
    pub fn vec(self, v: Vec2) -> Vec2 {
        match self.0 {
            Some(t) => v * t.scaling,
            None => v,
        }
    }

    /// A painter on an overlay layer (`order`, `id`: a drag ghost, a pick whip's line) drawn in the
    /// same coordinates as the panel, so what it paints lands where the panel's pointer is.
    pub fn overlay(self, ctx: &Context, order: egui::Order, id: Id) -> egui::Painter {
        let layer = LayerId::new(order, id);
        ctx.set_transform_layer(layer, self.0.map_or(TSTransform::IDENTITY, |t| t.inverse()));
        // Its clip is the window, in the panel's coordinates.
        let screen = ctx.content_rect();
        ctx.layer_painter(layer).with_clip_rect(Rect::from_two_pos(self.p(screen.min), self.p(screen.max)))
    }

    /// A point of the layer in window coordinates.
    pub fn window_p(self, p: Pos2) -> Pos2 {
        match self.0 {
            Some(t) => t.inverse() * p,
            None => p,
        }
    }

    /// A rectangle of the layer in window coordinates (to paint it on another, untransformed
    /// layer: a drag ghost).
    pub fn to_window(self, r: Rect) -> Rect {
        match self.0 {
            Some(t) => t.inverse() * r,
            None => r,
        }
    }

    /// Whether the layer is transformed (a host's canvas).
    pub fn transformed(self) -> bool {
        self.0.is_some()
    }

    /// Window points per layer point: the host's canvas zoom (1 when untransformed).
    pub fn scale(self) -> f32 {
        match self.0 {
            Some(t) if t.scaling.is_finite() && t.scaling > 0.0 => 1.0 / t.scaling,
            _ => 1.0,
        }
    }

    /// The scale a popup opened from this layer shows at: [`popup_scale`] of [`Lp::scale`].
    pub fn popup_scale(self) -> f32 {
        popup_scale(self.scale())
    }
}

/// The smallest and largest a popup opened in a host's canvas is drawn, times its normal size.
pub const POPUP_SCALE: (f32, f32) = (0.75, 1.5);

/// The scale of a popup opened from a card on a host's canvas (the Guides menu, zoom %,
/// resolution, channel, a right-click menu): the canvas zoom `canvas_scale`, clamped to
/// [`POPUP_SCALE`] so its text stays readable; it is anchored to its button every frame (Elias,
/// 29 Oct 2026: at 23 % the Guides menu was bigger than the whole card, at 120 % tiny next to a
/// huge toolbar, and it drifted from its button while the canvas zoomed).
pub fn popup_scale(canvas_scale: f32) -> f32 {
    if canvas_scale.is_finite() && canvas_scale > 0.0 { canvas_scale.clamp(POPUP_SCALE.0, POPUP_SCALE.1) } else { 1.0 }
}

/// Draw `layer` (a popup laid out in window coordinates) `k` times its size about the window
/// point `about`, which stays where it is (the popup's corner on its button).
pub fn scale_layer_about(ctx: &Context, layer: LayerId, about: Pos2, k: f32) {
    ctx.set_transform_layer(layer, TSTransform::new(about.to_vec2() * (1.0 - k), k));
}

/// The corner of `area` (a popup) on the side of `anchor` (its button, or the point it opened
/// at): the point it is scaled about, so the side touching the button stays on it.
pub fn corner_toward(area: Rect, anchor: Rect) -> Pos2 {
    let c = anchor.center();
    Pos2::new(if c.x <= area.center().x { area.min.x } else { area.max.x }, if c.y <= area.center().y { area.min.y } else { area.max.y })
}

/// Scale the open egui popup (menu or context menu) of `resp` with `resp`'s layer, as
/// [`popup_scale`] says. Call it right after showing the popup. Nothing happens in an
/// untransformed layer (EffectCraft's own window) or while the popup is closed. Its submenus
/// stay at their normal size.
pub fn scale_egui_popup(resp: &egui::Response) {
    let ctx = &resp.ctx;
    let lp = Lp::layer(ctx, resp.layer_id);
    if !lp.transformed() {
        return;
    }
    let id = egui::Popup::default_response_id(resp);
    if !egui::Popup::is_id_open(ctx, id) {
        return;
    }
    let Some(area) = ctx.memory(|m| m.area_rect(id)) else { return };
    // A context menu opens at the pointer, a menu at its button.
    let anchor = egui::Popup::position_of_id(ctx, id).map_or_else(|| lp.to_window(resp.interact_rect), Rect::from_pos);
    scale_layer_about(ctx, LayerId::new(egui::Order::Foreground, id), corner_toward(area, anchor), lp.popup_scale());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untransformed_layers_change_nothing() {
        let ctx = Context::default();
        let lp = Lp::layer(&ctx, LayerId::background());
        assert_eq!(lp.p(Pos2::new(3.0, 4.0)), Pos2::new(3.0, 4.0));
        assert_eq!(lp.vec(Vec2::new(2.0, 2.0)), Vec2::new(2.0, 2.0));
        assert_eq!(Lp::panel(&ctx), Lp(None));
    }

    #[test]
    fn transformed_layers_get_their_own_coordinates() {
        let ctx = Context::default();
        let layer = LayerId::new(egui::Order::Middle, Id::new("scaled"));
        ctx.set_transform_layer(layer, TSTransform::new(Vec2::new(100.0, 50.0), 0.5));
        let lp = Lp::layer(&ctx, layer);
        // Window (110, 60) is 10, 10 from the layer's origin at half size: 20, 20 in it.
        assert_eq!(lp.p(Pos2::new(110.0, 60.0)), Pos2::new(20.0, 20.0));
        assert_eq!(lp.vec(Vec2::new(5.0, 5.0)), Vec2::new(10.0, 10.0));
        let back = lp.to_window(Rect::from_min_size(Pos2::new(20.0, 20.0), Vec2::new(10.0, 10.0)));
        assert_eq!(back, Rect::from_min_size(Pos2::new(110.0, 60.0), Vec2::new(5.0, 5.0)));
        assert_eq!(lp.window_p(Pos2::new(20.0, 20.0)), Pos2::new(110.0, 60.0));
    }
}
