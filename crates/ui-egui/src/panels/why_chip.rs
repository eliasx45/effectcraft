//! The Why? strip under the Timeline (batch 1, CX6).

use egui::Rect;

use crate::EffectcraftApp;

/// Draw the Why? strip at the bottom of the Timeline `rect`; its height (0 when closed).
pub fn strip(app: &mut EffectcraftApp, ui: &mut egui::Ui, rect: Rect) -> f32 {
    let _ = (app, ui, rect);
    0.0
}
