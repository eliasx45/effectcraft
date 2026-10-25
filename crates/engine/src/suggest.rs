//! "Did you mean …?" suggestions for agent-facing errors: the closest command ids, effect names,
//! property ids or tool arguments to a mistyped one (case-insensitive edit distance, plus
//! substring matches).

/// Edit distance between `a` and `b` (case-insensitive, by `char`).
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        if let Some(c0) = cur.first_mut() {
            *c0 = i + 1;
        }
        for (j, cb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(0) + usize::from(ca != cb);
            let del = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let ins = cur.get(j).copied().unwrap_or(0) + 1;
            if let Some(c) = cur.get_mut(j + 1) {
                *c = sub.min(del).min(ins);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.last().copied().unwrap_or(0)
}

/// Up to `n` candidates closest to `word`: plausible typos (distance at most a third of the
/// longer word, min 2) and candidates containing `word` (or contained in it), best first.
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>, n: usize) -> Vec<&'a str> {
    let w = word.to_lowercase();
    if w.is_empty() {
        return vec![];
    }
    let mut scored: Vec<(usize, &str)> = candidates
        .into_iter()
        .filter_map(|c| {
            let lc = c.to_lowercase();
            let d = distance(&w, &lc);
            let limit = (w.chars().count().max(lc.chars().count()) / 3).max(2);
            let contains = lc.contains(&w) || (w.len() >= 3 && w.contains(&lc) && lc.len() >= 3);
            // Substring matches rank after close typos but before the cut-off.
            (d <= limit || contains).then_some((if d <= limit { d } else { limit + 1 }, c))
        })
        .collect();
    // Stable: on a tie the earlier candidate wins (callers list the preferred spelling first).
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
    let mut out: Vec<&str> = vec![];
    for (_, c) in scored {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(c)) {
            out.push(c);
        }
        if out.len() >= n {
            break;
        }
    }
    out
}

/// `"; did you mean `a`, `b` or `c`?"`, or an empty string when nothing is close.
pub fn did_you_mean<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    let c = closest(word, candidates, 3);
    match c.as_slice() {
        [] => String::new(),
        [one] => format!("; did you mean `{one}`?"),
        [init @ .., last] => format!("; did you mean {} or `{last}`?", init.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")),
    }
}

/// `a, b, c, … (N more)`: at most `cap` items of a list.
pub fn capped_list<'a>(items: impl IntoIterator<Item = &'a str>, cap: usize) -> String {
    let all: Vec<&str> = items.into_iter().collect();
    let shown = all.iter().take(cap).copied().collect::<Vec<_>>().join(", ");
    if all.len() > cap { format!("{shown}, … ({} more)", all.len() - cap) } else { shown }
}

/// The suffix of an unknown-command error: the closest command ids.
pub(crate) fn command_hint(id: &str) -> String {
    let ids = crate::commands::command_specs().iter().map(|c| c.id);
    let s = did_you_mean(id, ids);
    if s.is_empty() { "; list them with command.list {filter}".into() } else { s }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn closest_matches_typos_and_substrings() {
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(closest("inputBlack", ["inBlack", "inWhite", "gamma", "outBlack"], 3), vec!["inBlack", "outBlack"]);
        assert_eq!(closest("layer.newSoild", ["layer.newSolid", "layer.newText", "comp.new"], 1), vec!["layer.newSolid"]);
        assert!(closest("zzz", ["inBlack"], 3).is_empty());
        assert_eq!(did_you_mean("comand", ["command", "id"]), "; did you mean `command`?");
        assert_eq!(capped_list(["a", "b", "c"], 2), "a, b, … (1 more)");
    }

    #[test]
    fn unknown_command_suggests_ids() {
        let mut s = crate::Session::default();
        let e = s.execute("layer.newSoild", json!({})).unwrap_err().to_string();
        assert!(e.contains("did you mean") && e.contains("`layer.newSolid`"), "{e}");
    }

    /// A comp with a solid carrying Levels; returns the layer id.
    fn levels_layer(s: &mut crate::Session) -> u64 {
        s.execute("comp.new", json!({"name": "C", "width": 100, "height": 100})).unwrap();
        let l = s.execute("layer.newSolid", json!({})).unwrap()["layer"].as_u64().unwrap();
        s.execute_checked("effect.apply", json!({"layers": [l], "effect": "Levels"})).unwrap();
        l
    }

    #[test]
    fn prop_set_reports_clamped_and_coerced_values() {
        let mut s = crate::Session::default();
        let l = levels_layer(&mut s);
        let set = |s: &mut crate::Session, path: &str, v: serde_json::Value| s.execute_checked("prop.set", json!({"layer": l, "path": path, "value": v}));
        // Out of range: stored clamped as before, and said so.
        let r = set(&mut s, "effects/#1/inBlack", json!(20)).unwrap();
        assert_eq!(r["value"], 2.0);
        let w = r["warnings"][0].as_str().unwrap();
        assert!(w.starts_with("inBlack: 20 is outside the valid range -1 to 2 (slider 0 to 1); clamped to 2"), "{w}");
        assert!(set(&mut s, "effects/#1/inBlack", json!(-5)).unwrap()["warnings"][0].as_str().unwrap().contains("clamped to -1"));
        // In range: the plain value, no warnings.
        assert_eq!(set(&mut s, "effects/#1/inBlack", json!(0.25)).unwrap(), json!(0.25));
        // A number fills a vector.
        let r = set(&mut s, "transform/position", json!(5)).unwrap();
        assert_eq!(r["value"], json!([5.0, 5.0, 5.0]));
        assert_eq!(r["warnings"][0], "position: number 5 expanded to [5, 5, 5]");
        // prop.addKey reports too.
        let r = s.execute_checked("prop.addKey", json!({"layer": l, "path": "effects/#1/gamma", "time": 0, "value": 50})).unwrap();
        assert_eq!(r["keys"], 1);
        assert!(r["warnings"][0].as_str().unwrap().contains("clamped to 10"), "{r}");
        // A batch gathers its steps' warnings.
        let r = s
            .execute_checked("engine.batch", json!({"steps": [{"command": "prop.set", "params": {"layer": l, "path": "effects/#1/outWhite", "value": 9}}]}))
            .unwrap();
        assert!(r["warnings"][0].as_str().unwrap().starts_with("step 1 (prop.set): outWhite: 9 is outside"), "{r}");
    }

    #[test]
    fn popups_take_names_and_errors_list_the_choices() {
        let mut s = crate::Session::default();
        let l = levels_layer(&mut s);
        let r = s.execute_checked("prop.set", json!({"layer": l, "path": "effects/#1/channel", "value": "red"})).unwrap();
        assert_eq!(r, json!(1));
        let e = s.execute_checked("prop.set", json!({"layer": l, "path": "effects/#1/channel", "value": "Purple"})).unwrap_err().to_string();
        assert!(e.contains("choose one of: 0 = RGB, 1 = Red, 2 = Green, 3 = Blue, 4 = Alpha"), "{e}");
        let r = s.execute_checked("prop.set", json!({"layer": l, "path": "effects/#1/channel", "value": 9})).unwrap();
        assert!(r["warnings"][0].as_str().unwrap().contains("not an option"), "{r}");
        let e = s.execute_checked("prop.set", json!({"layer": l, "path": "transform/opacity", "value": "half"})).unwrap_err().to_string();
        assert!(e.contains("expected a number"), "{e}");
    }

    #[test]
    fn unknown_paths_and_effects_suggest_fixes() {
        let mut s = crate::Session::default();
        let l = levels_layer(&mut s);
        let e = s.execute_checked("prop.set", json!({"layer": l, "path": "effects/#1/inputBlack", "value": 0})).unwrap_err().to_string();
        assert!(e.contains("`effects/#1` (Levels) has no `inputBlack`; did you mean `inBlack`") && e.contains("Valid here: ") && e.contains("gamma"), "{e}");
        let e = s.execute_checked("prop.get", json!({"layer": l, "path": "effects/#3/gamma"})).unwrap_err().to_string();
        assert!(e.contains("Valid here: #1 (Levels)"), "{e}");
        let e = s.execute_checked("prop.get", json!({"layer": l, "path": "transfrom/opacity"})).unwrap_err().to_string();
        assert!(e.contains("did you mean `transform`"), "{e}");
        let e = s.execute_checked("prop.get", json!({"layer": l, "path": "effects/#1"})).unwrap_err().to_string();
        assert!(e.contains("is a group, not a property"), "{e}");
        let e = s.execute_checked("effect.apply", json!({"layers": [l], "effect": "Gausian Blur"})).unwrap_err().to_string();
        assert!(e.contains("did you mean `Gaussian Blur`"), "{e}");
    }

    /// Keys selected on Deep Glow (Native), effect #2 after Gaussian Blur: the path `keys.info`
    /// gives for them, and the match path with the dotted match id, both work with `prop.get`.
    #[test]
    fn selected_keys_on_a_second_dotted_effect_resolve() {
        let mut s = crate::Session::default();
        s.execute("comp.new", json!({"name": "C", "width": 100, "height": 100})).unwrap();
        let l = s.execute("layer.newSolid", json!({})).unwrap()["layer"].as_u64().unwrap();
        s.execute_checked("effect.apply", json!({"layers": [l], "effect": "Gaussian Blur"})).unwrap();
        // One effect: its dotted match id resolves.
        s.execute_checked("prop.get", json!({"layer": l, "path": "effects/ec.blur.gaussian/blurriness"})).unwrap();
        s.execute_checked("effect.apply", json!({"layers": [l], "effect": "ec.stylize.deepglow"})).unwrap();
        let mut keys = vec![];
        for param in ["PEDG-0001", "glowExposure"] {
            for (t, v) in [(0.0, 1.0), (1.0, 2.0)] {
                s.execute_checked("prop.addKey", json!({"layer": l, "path": format!("effects/#2/{param}"), "time": t, "value": v})).unwrap();
            }
            let uid = s.execute_checked("prop.get", json!({"layer": l, "path": format!("effects/#2/{param}")})).unwrap()["uid"].clone();
            keys.extend([json!({"layer": l, "prop": uid, "time": 0.0}), json!({"layer": l, "prop": uid, "time": 1.0})]);
        }
        s.execute_checked("keys.select", json!({"keys": keys})).unwrap();
        let info = s.execute_checked("keys.info", json!({})).unwrap();
        let info = info.as_array().unwrap();
        assert_eq!(info.len(), 4, "{info:?}");
        for k in info {
            assert!(k["path"].as_str().unwrap().starts_with("effects/#2/"), "{k}");
            assert_eq!(k["matchPath"].as_str().unwrap().split('/').nth(1), Some("ec.stylize.deepglow"), "{k}");
            for key in ["path", "matchPath"] {
                let r = s.execute_checked("prop.get", json!({"layer": l, "path": k[key]}));
                assert!(r.is_ok(), "{key} {}: {r:?}", k[key]);
            }
        }
        s.execute_checked("prop.get", json!({"layer": l, "path": "effects/ec.stylize.deepglow/PEDG-0001"})).unwrap();
        s.execute_checked("prop.get", json!({"layer": l, "path": "effects/Deep Glow (Native)/exposure"})).unwrap();
        // A dotted id that names nothing is reported whole.
        let e = s.execute_checked("prop.get", json!({"layer": l, "path": "effects/ec.stylize.nope/exposure"})).unwrap_err().to_string();
        assert!(e.contains("has no `ec.stylize.nope`") && e.contains("#2 (Deep Glow (Native))"), "{e}");
    }

    #[test]
    fn mutations_return_what_changed() {
        let mut s = crate::Session::default();
        let l = levels_layer(&mut s);
        s.execute_checked("effect.apply", json!({"layers": [l], "effect": "Invert"})).unwrap();
        let r = s.execute_checked("effect.reorder", json!({"layer": l, "effect": "Invert", "index": 1})).unwrap();
        assert_eq!(r["layer"], l);
        assert_eq!(r["effects"][0]["index"], 1);
        assert_eq!(r["effects"][0]["name"], "Invert");
        assert_eq!(r["effects"][1]["name"], "Levels");
        let m = s.execute_checked("layer.addMask", json!({"layer": l})).unwrap();
        assert_eq!(m["path"], "masks/#1");
        assert!(m["mask"].as_u64().is_some());
        s.execute_checked("prop.set", json!({"layer": l, "path": format!("{}/feather", m["path"].as_str().unwrap()), "value": [4, 4]})).unwrap();
        let t = s.execute_checked("layer.newText", json!({"text": "Hi"})).unwrap()["layer"].as_u64().unwrap();
        // Explicit targets come back resolved (the comp and layer the command acted on).
        let comp = s.active_comp_id().unwrap().0;
        assert_eq!(
            s.execute_checked("layer.setText", json!({"layer": t, "text": "Hello"})).unwrap(),
            json!({"layer": t, "text": "Hello", "resolved": {"comp": comp, "layer": t}})
        );
    }

    #[test]
    fn schemas_type_index_params_and_listings_have_a_brief_mode() {
        let schema = |id: &str| crate::commands::params_schema(crate::commands::find(id).unwrap());
        let reorder = schema("effect.reorder");
        assert_eq!(reorder["properties"]["index"]["type"], "integer");
        assert!(reorder["properties"]["index"]["description"].as_str().unwrap().contains("1-based"));
        let vertex = schema("mask.setVertex");
        assert_eq!(vertex["properties"]["index"]["type"], "integer");
        assert!(vertex["properties"]["index"]["description"].as_str().unwrap().contains("0-based"));
        assert_eq!(schema("prop.set")["properties"]["layer"]["type"], json!(["integer", "string"]));
        assert_eq!(schema("layer.select")["properties"]["layers"]["type"], "array");

        let mut s = crate::Session::default();
        let brief = s.execute_checked("command.list", json!({"brief": true})).unwrap();
        let full = s.execute_checked("command.list", json!({})).unwrap();
        assert_eq!(brief.as_array().unwrap().len(), full.as_array().unwrap().len());
        assert!(brief.as_array().unwrap().iter().all(|c| c.as_object().unwrap().len() == 2 && c["label"].is_string()));
        assert!(brief.to_string().len() * 3 < full.to_string().len());
        let fx = s.execute_checked("effect.list", json!({"brief": true})).unwrap();
        assert!(fx.as_array().unwrap().iter().all(|e| e.as_object().unwrap().len() == 2));
        let levels = s.execute_checked("effect.list", json!({"filter": "levels"})).unwrap();
        let levels = levels.as_array().unwrap().iter().find(|e| e["id"] == "ec.color.levels").unwrap().clone();
        let param = |id: &str| levels["params"].as_array().unwrap().iter().find(|p| p["id"] == id).unwrap().clone();
        assert_eq!(param("inBlack")["range"], json!([-1.0, 2.0]));
        assert_eq!(param("channel")["options"][1]["label"], "Red");
    }
}
