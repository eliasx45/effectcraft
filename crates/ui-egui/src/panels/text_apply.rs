//! `text.apply` from the UI (batch 1 integration): the quick row's Text buttons (AX9) and the
//! Script tab's phrase popover (SX6) run lane D's command through here, so both also run its
//! `followUp` (with `glow` and no glow layer in the project, `text.apply` returns
//! `followUp: [{command: "motionLibrary.apply", params: {id}}]`: the Motion Library's plug applies
//! the "Caption glow + double shadow" style to the layers `text.apply` left selected), and both
//! list the presets the same way (`text.presetFamilies`).

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

/// The text presets as (short name, label), from `text.presetFamilies`
/// (`families[].variants[].{short, name}`), in family order.
pub fn preset_list(app: &mut EffectcraftApp) -> Vec<(String, String)> {
    let Ok(r) = app.session.execute("text.presetFamilies", json!({})) else { return vec![] };
    parse_families(&r)
}

fn parse_families(r: &Value) -> Vec<(String, String)> {
    let mut out = vec![];
    for f in r.get("families").and_then(Value::as_array).into_iter().flatten() {
        for v in f.get("variants").and_then(Value::as_array).into_iter().flatten() {
            let Some(short) = v.get("short").and_then(Value::as_str) else { continue };
            let name = v.get("name").and_then(Value::as_str).unwrap_or(short);
            if out.len() < 64 {
                out.push((short.to_string(), name.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_flatten_to_short_names() {
        let r = json!({"families": [
            {"family": "T1", "name": "Fade up", "variants": [{"short": "T1b", "name": "T1b Fade up · mid"}]},
            {"family": "T3", "name": "Pop", "variants": [{"short": "T3a", "name": "T3a Pop · near"}, {"short": "T3b"}]},
        ]});
        let got = parse_families(&r);
        assert_eq!(got, [("T1b".to_string(), "T1b Fade up · mid".to_string()), ("T3a".into(), "T3a Pop · near".into()), ("T3b".into(), "T3b".into())]);
        assert!(parse_families(&Value::Null).is_empty());
    }
}
