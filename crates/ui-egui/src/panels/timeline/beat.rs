//! Timeline marks for batch 1: the linked-graph tag on a move's first key (AX3, "Fast In ·
//! linked") and the off-beat dot and syllable snap (AX6).

use effectcraft_engine::commands::graph_cmds::linked_name;
use effectcraft_engine::project::{Layer, Property};
use egui::{Align2, Color32, Painter, pos2};

use super::TMap;
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
