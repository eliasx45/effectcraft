//! Cards in the UI (batch 1, CX1): the card wheel ("Make card ▾" in the quick row, lane A, calls
//! [`open_card_wheel`]). Its slices are the card styles (`card.styles`: Devin 2:3 · 8.6 %, Reel
//! 9:16, Post 4:5, Square 1:1, Very round, Sharp, then the saved ones) and "+ Save mine".
//! Picking a style runs `card.make` (pictures become cards, cards are restyled) or `card.style`
//! (every selected layer is a card already); "+ Save mine" runs `card.saveStyle`.
//!
//! Everything is a command, so agents use `card.make {layers, style}` directly.

use egui::Pos2;
use serde_json::{Value, json};

use crate::EffectcraftApp;
use crate::wheel::{self, WheelItem};

/// The card wheel's key (`wheel.card.<style id>` automation ids).
pub const CARD_WHEEL: &str = "card";

fn items_id() -> egui::Id {
    egui::Id::new("cards-wheel-items")
}

/// The wheel's slices for `styles` (a `card.styles` reply), then "+ Save mine".
fn wheel_items(styles: &Value) -> Vec<WheelItem> {
    let mut items: Vec<WheelItem> = styles["styles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|st| {
                    let id = st.get("id")?.as_str()?;
                    let label = st.get("label")?.as_str()?;
                    let hint = format!(
                        "{} · corners {} % of the width{}",
                        st.get("aspect").and_then(Value::as_str).unwrap_or(""),
                        st.get("radius").and_then(Value::as_f64).unwrap_or(0.0),
                        if st.get("saved").and_then(Value::as_bool) == Some(true) { " · Motion Library ▸ Elias ▸ Cards" } else { "" }
                    );
                    Some(WheelItem::new(id, label, &hint))
                })
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        items.push(WheelItem::new("devin", "Devin 2:3 · 8.6 %", "2:3 · corners 8.6 % of the width"));
    }
    items.push(WheelItem::new("save", "+ Save mine", "Save the selected card's style to Motion Library ▸ Elias ▸ Cards"));
    items
}

/// Open the card wheel around `center` (the "Make card ▾" button, lane A's quick row).
pub fn open_card_wheel(ctx: &egui::Context, center: Pos2) {
    let items: Vec<WheelItem> = ctx.data(|d| d.get_temp::<Vec<WheelItem>>(items_id())).unwrap_or_else(|| wheel_items(&Value::Null));
    wheel::open(ctx, CARD_WHEEL, center, "CARD", items, None);
}

/// Once per frame (from the app's frame, next to `wheel::paint`): keeps the wheel's styles
/// current and runs a picked slice.
pub fn frame(app: &mut EffectcraftApp, ctx: &egui::Context) {
    // The styles change with the settings (a saved style): read them again then.
    let rev_id = egui::Id::new("cards-wheel-prefs-rev");
    let rev = app.session.prefs_revision;
    if ctx.data(|d| d.get_temp::<u64>(rev_id)) != Some(rev) || ctx.data(|d| d.get_temp::<Vec<WheelItem>>(items_id())).is_none() {
        // A query: not journaled, no undo step.
        let styles = app.session.execute("card.styles", json!({})).unwrap_or(Value::Null);
        let items = wheel_items(&styles);
        ctx.data_mut(|d| {
            d.insert_temp(rev_id, rev);
            d.insert_temp(items_id(), items);
        });
    }
    if let Some(pick) = wheel::take_pick(ctx, CARD_WHEEL) {
        let r = pick_style(app, ctx, &pick);
        match r {
            Ok(msg) => app.toast = Some((msg, ctx.input(|i| i.time))),
            Err(e) => app.ui.status = e,
        }
    }
}

/// Run the picked slice on the selected layers.
fn pick_style(app: &mut EffectcraftApp, ctx: &egui::Context, pick: &str) -> Result<String, String> {
    if app.session.state.selected_layers.is_empty() {
        return Err("Select a card or a picture first".into());
    }
    if pick == "save" {
        let r = crate::menus::invoke(app, ctx, "card.saveStyle", json!({}))?;
        let label = r["style"]["label"].as_str().unwrap_or("your style");
        return Ok(format!("Saved to Motion Library ▸ Elias ▸ Cards as “{label}”: it’s on the wheel now"));
    }
    // Restyle when every selected layer is a card already; otherwise make cards (which also
    // restyles the cards among them).
    let all_cards = app.session.active_comp().is_some_and(|c| {
        app.session.state.selected_layers.iter().all(|id| c.layer(*id).is_some_and(effectcraft_engine::commands::cards::is_card))
    });
    let id = if all_cards { "card.style" } else { "card.make" };
    let r = crate::menus::invoke(app, ctx, id, json!({"style": pick}))?;
    let label = r["style"]["label"].as_str().unwrap_or(pick);
    let n = r["cards"].as_array().map(Vec::len).unwrap_or(0);
    let skipped = r["skipped"].as_array().map(Vec::len).unwrap_or(0);
    let what = if all_cards { "Restyled" } else { "Card made" };
    Ok(if skipped > 0 { format!("{what}: {label} on {n} layer(s); {skipped} skipped") } else { format!("{what}: {label} · from Motion Library ▸ Elias ▸ Cards") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_lists_styles_then_save() {
        let styles = json!({"styles": [{"id": "devin", "label": "Devin 2:3 · 8.6 %", "aspect": "2:3", "radius": 8.6, "saved": false}, {"id": "my-1", "label": "Mine", "aspect": "2:3", "radius": 12.0, "saved": true}]});
        let items = wheel_items(&styles);
        assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["devin", "my-1", "save"]);
        assert!(items[1].hint.contains("Elias ▸ Cards"));
        // Nothing known yet: Devin's style and Save.
        assert_eq!(wheel_items(&Value::Null).len(), 2);
    }
}
