//! `text.apply` from the UI (batch 1 integration): the quick row's Text buttons (AX9) and the
//! Script tab's phrase popover (SX6) run lane D's command through here, so both also run its
//! `followUp` (with `glow` and no glow layer in the project, `text.apply` returns
//! `followUp: [{command: "motionLibrary.apply", params: {id}}]`: the Motion Library's plug applies
//! the "Caption glow + double shadow" style to the layers `text.apply` left selected), and both
//! list the presets the same way (`text.presetFamilies`: Elias's preset cards, CX8c).

use serde_json::{Value, json};

use crate::EffectcraftApp;

/// Most follow-up steps run after one `text.apply`.
const MAX_FOLLOW_UPS: usize = 8;

/// Runs `text.apply {params}`, then each of its `followUp` steps. The first error is returned
/// (a follow-up's error after the preset applied says so).
pub fn run(app: &mut EffectcraftApp, ctx: &egui::Context, params: Value) -> Result<Value, String> {
    let r = crate::menus::invoke(app, ctx, "text.apply", params)?;
    run_follow_ups(app, ctx, &r)?;
    Ok(r)
}

/// Runs the `followUp` steps of a command's result (`[{command, params}]`, or one such object).
pub fn run_follow_ups(app: &mut EffectcraftApp, ctx: &egui::Context, result: &Value) -> Result<(), String> {
    let steps: Vec<Value> = match result.get("followUp") {
        Some(Value::Array(a)) => a.clone(),
        Some(v @ Value::Object(_)) => vec![v.clone()],
        _ => return Ok(()),
    };
    for step in steps.iter().take(MAX_FOLLOW_UPS) {
        let Some(id) = step.get("command").and_then(Value::as_str) else { continue };
        let params = step.get("params").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
        crate::menus::invoke(app, ctx, id, params).map_err(|e| format!("The preset is on; {id} after it failed: {e}"))?;
    }
    Ok(())
}

/// The text presets as (name, label), from `text.presetFamilies` (`presets[].{name, controls}`):
/// Elias's preset cards in Motion Library ▸ Elias ▸ Text animations (CX8c). The label names the
/// preset's controls.
pub fn preset_list(app: &mut EffectcraftApp) -> Vec<(String, String)> {
    let Ok(r) = app.session.execute("text.presetFamilies", json!({})) else { return vec![] };
    parse_presets(&r)
}

/// The old near / mid / far chips as travel in pixels (the engine's `text_presets::TRAVEL`): the
/// label a distance chip shows.
pub fn distance_label(d: &str) -> String {
    let px = match d {
        "near" => 40,
        "far" => 150,
        _ => 90,
    };
    format!("{d} {px}px")
}

fn parse_presets(r: &Value) -> Vec<(String, String)> {
    let mut out = vec![];
    for p in r.get("presets").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = p.get("name").and_then(Value::as_str) else { continue };
        // One button per name: `text.presetFamilies` already keeps the newest card of a repeated
        // name; a listing that still repeats one keeps its first.
        if out.iter().any(|(n, _): &(String, String)| n.trim().eq_ignore_ascii_case(name.trim())) {
            continue;
        }
        let controls: Vec<&str> =
            p.get("controls").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("name").and_then(Value::as_str)).collect();
        let label = if controls.is_empty() { name.to_string() } else { format!("{name} · {}", controls.join(", ")) };
        if out.len() < 64 {
            out.push((name.to_string(), label));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_list_their_names_and_controls() {
        let r = json!({"presets": [
            {"name": "Slide In", "controls": [{"name": "Travel Distance (px)"}, {"name": "Animate By"}]},
            {"name": "Bounce In", "source": "motion"},
        ]});
        let got = parse_presets(&r);
        assert_eq!(got, [("Slide In".to_string(), "Slide In · Travel Distance (px), Animate By".to_string()), ("Bounce In".into(), "Bounce In".into())]);
        assert!(parse_presets(&Value::Null).is_empty());
        assert_eq!(distance_label("far"), "far 150px");
    }

    /// Batch 1: "Slide In ↑" showed twice (an old card and its remake in the library).
    #[test]
    fn a_repeated_name_is_one_button() {
        let r = json!({"presets": [{"name": "Slide In", "id": "new"}, {"name": "slide in ", "id": "old"}, {"name": "Bounce In"}]});
        let names: Vec<String> = parse_presets(&r).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Slide In", "Bounce In"]);
    }
}
