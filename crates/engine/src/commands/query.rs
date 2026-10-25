//! Read-only queries for agents (not journaled).

use effectcraft_project::{GroupKind, ItemKind, Layer, Node, PropGroup, Property, Uid};
use serde_json::{Value, json};

use super::{CommandSpec, b_p, comp_id, layer_p, str_p};
use crate::{EngineError, Result, Session, query};

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    let filter = str_p(p, "filter").map(str::to_ascii_lowercase);
    let only = b_p(p, "enabledOnly").unwrap_or(false);
    let schemas = b_p(p, "schemas").unwrap_or(false);
    let brief = b_p(p, "brief").unwrap_or(false);
    let v: Vec<Value> = super::command_specs()
        .iter()
        .filter(|c| filter.as_ref().is_none_or(|f| c.id.to_ascii_lowercase().contains(f) || c.label.to_ascii_lowercase().contains(f)))
        .filter_map(|c| {
            let en = (c.enabled)(s);
            if only && en.is_err() {
                return None;
            }
            if brief {
                // The cheap index: ids and labels only (command.describe has the rest).
                return Some(json!({"id": c.id, "label": c.label}));
            }
            let mut o =
                json!({"id": c.id, "label": c.label, "menu": c.menu, "shortcut": c.shortcut, "params": c.params, "enabled": en.is_ok(), "why": en.err()});
            if schemas {
                o["schema"] = super::params_schema(c);
            }
            Some(o)
        })
        .collect();
    Ok(json!(v))
}

pub(crate) fn summary(s: &mut Session, _: &Value) -> Result<Value> {
    let items: Vec<Value> = s
        .project
        .items
        .values()
        .map(|i| {
            let mut o = json!({"id": i.id.0, "name": i.name, "type": i.type_name(), "label": i.label.name(), "parent": i.parent.map(|p| p.0)});
            if let Some((w, h)) = i.dimensions() {
                o["size"] = json!([w, h]);
            }
            if let Some(d) = i.duration() {
                o["duration"] = json!(d.seconds());
            }
            if let ItemKind::Comp(c) = &i.kind {
                o["layers"] = json!(c.layers.len());
                o["frameRate"] = json!(c.frame_rate.as_f64());
            }
            // Footage that shows no picture although its file is there (a codec or profile the
            // decoders lack): why, so an agent can tell the user.
            if let ItemKind::Footage(f) = &i.kind
                && let Some(why) = s.footage.problem(f)
            {
                o["problem"] = json!(why);
            }
            o
        })
        .collect();
    let mut out = json!({
        "path": s.path,
        "dirty": s.is_dirty(),
        // Changes with every project edit (also undo/redo), never for queries: a cheap "has
        // anything changed" check for agents (MCP resource subscriptions).
        "revision": s.revision,
        "bitDepth": s.project.settings.bit_depth.label(),
        "renderer": super::file::backend_status(s),
        "items": items,
        "activeComp": s.state.active_comp.map(|c| c.0),
        "time": s.time().seconds(),
        "selectedLayers": s.state.selected_layers.iter().map(|l| l.0).collect::<Vec<_>>(),
        "undo": s.history.undo.iter().map(|u| u.0.clone()).collect::<Vec<_>>(),
    });
    crate::build_info::session_identity(s, &mut out);
    Ok(out)
}

fn layer_json(l: &Layer, idx: usize) -> Value {
    json!({
        "id": l.id.0, "index": idx, "name": l.name, "type": l.source.type_name(), "label": l.label.name(),
        "in": l.in_point.seconds(), "out": l.out_point.seconds(), "start": l.start_time.seconds(), "stretch": l.stretch,
        "blendMode": l.blend_mode.label(), "parent": l.parent.map(|p| p.0),
        "trackMatte": l.track_matte.map(|m| json!({"layer": m.layer.0, "kind": m.kind.label()})),
        "switches": serde_json::to_value(&l.switches).unwrap_or_default(),
    })
}

fn comp_info(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let name = s.project.item(cid).map(|i| i.name.clone()).unwrap_or_default();
    Ok(json!({
        "id": cid.0, "comp": cid.0, "revision": s.revision, "time": s.time_of(cid).seconds(), "timeBase": "comp",
        "name": name, "width": c.width, "height": c.height, "frameRate": c.frame_rate.as_f64(), "duration": c.duration.seconds(),
        "workArea": [c.work_area.0.seconds(), c.work_area.1.seconds()], "background": c.background,
        "preserveFrameRate": c.preserve_frame_rate, "preserveResolution": c.preserve_resolution,
        "resolution": c.resolution.label(), "resolutionFactor": c.resolution.factor(),
        "layers": c.layers.iter().enumerate().map(|(i, l)| layer_json(l, i + 1)).collect::<Vec<_>>(),
    }))
}

/// The path segment that addresses child `i` of `g` (see `effectcraft_project::parse_path`): the match
/// id (with `#n` when it is not the first sibling matching it), or `#index` when the match id is not
/// path-safe (effects and other ids containing `.`).
pub(crate) fn segment(g: &PropGroup, i: usize) -> String {
    let c = &g.children[i];
    let m = c.match_id();
    if m.is_empty() || m.contains(['/', '.', '#', '@']) || m.parse::<u64>().is_ok() {
        return format!("#{}", i + 1);
    }
    let n = g.children[..i].iter().filter(|x| x.match_id() == m || x.name().eq_ignore_ascii_case(m)).count() + 1;
    if n == 1 { m.to_string() } else { format!("{m}#{n}") }
}

const MAX_TREE_DEPTH: usize = 64;

/// The path `layer.tree` gives the node with `uid` (`effects/#2/PEDG2-0001`): built from
/// [`segment`], so an effect is addressed by its index, never by its dotted match id.
pub(crate) fn tree_path_of(g: &PropGroup, uid: Uid) -> Option<String> {
    fn walk(g: &PropGroup, uid: Uid, depth: usize) -> Option<String> {
        for (i, c) in g.children.iter().enumerate() {
            let seg = segment(g, i);
            if c.uid() == uid {
                return Some(seg);
            }
            if let Node::Group(sub) = c
                && depth < MAX_TREE_DEPTH
                && let Some(rest) = walk(sub, uid, depth + 1)
            {
                return Some(format!("{seg}/{rest}"));
            }
        }
        None
    }
    walk(g, uid, 0)
}

/// Local group switches accumulate down to leaves; a property has no switch of its own.
pub(crate) fn property_effective_enabled(root: &PropGroup, uid: Uid) -> Option<bool> {
    fn find(g: &PropGroup, uid: Uid, parent_enabled: bool, depth: usize) -> Option<bool> {
        let enabled = parent_enabled && g.enabled;
        for child in &g.children {
            match child {
                Node::Prop(p) if p.uid == uid => return Some(enabled),
                Node::Group(child) if depth < MAX_TREE_DEPTH => {
                    if let Some(found) = find(child, uid, enabled, depth + 1) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }
    find(root, uid, true, 0)
}

/// Shared with prop.get: keep the raw sample and report expression fallback/errors explicitly.
pub(crate) fn expression_readback(s: &Session, ctx: &crate::render::EvalCtx<'_>, l: &Layer, pr: &Property, out: &mut Value) {
    let raw = pr.value_at(l.layer_time(ctx.time));
    out["rawValue"] = raw.to_json();
    let Some(e) = &pr.expr else { return };
    out["expression"] = json!(e.text);
    out["expressionEnabled"] = json!(e.enabled);
    out["expressionEffectiveEnabled"] = json!(pr.has_expression() && out.get("effectiveEnabled").and_then(Value::as_bool).unwrap_or(true));
    let mut evaluated = raw.clone();
    if pr.has_expression() {
        match ctx.expr {
            Some(host) => match host.eval(ctx, l, pr, &raw) {
                Ok(value) => evaluated = value,
                Err(error) => out["expressionError"] = json!(error),
            },
            None => out["expressionError"] = json!("expression engine unavailable"),
        }
    } else if !e.enabled
        && !e.text.trim().is_empty()
        && let Some(error) = s.expr_check.and_then(|check| check(&e.text).err())
    {
        out["expressionError"] = json!(error);
    }
    out["evaluated"] = evaluated.to_json();
    out["evaluatedValue"] = evaluated.to_json();
}

fn group_json(s: &Session, ctx: &crate::render::EvalCtx<'_>, l: &Layer, max_depth: usize) -> Value {
    fn group(g: &PropGroup, path: &str, depth: usize, parent_enabled: bool, sample: &impl Fn(&Property, &str, bool) -> Value, max_depth: usize) -> Value {
        let effective_enabled = parent_enabled && g.enabled;
        let children: Vec<Value> = if depth >= max_depth {
            vec![]
        } else {
            g.children
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let seg = segment(g, i);
                    let cpath = if path.is_empty() { seg } else { format!("{path}/{seg}") };
                    match c {
                        Node::Prop(p) => sample(p, &cpath, effective_enabled),
                        Node::Group(g) => group(g, &cpath, depth + 1, effective_enabled, sample, max_depth),
                    }
                })
                .collect()
        };
        let mut o = json!({"path": path, "uid": g.uid, "match": g.match_id, "name": g.name, "enabled": g.enabled, "effectiveEnabled": effective_enabled, "children": children});
        if depth >= max_depth && !g.children.is_empty() {
            o["truncated"] = json!(true);
        }
        match &g.kind {
            GroupKind::Mask { mode, inverted, .. } => {
                o["mask"] = json!({"mode": mode.label(), "inverted": inverted});
            }
            GroupKind::Effect { effect } => {
                o["effect"] = json!(effect);
            }
            _ => {}
        }
        o
    }
    let sample = |p: &Property, path: &str, effective_enabled: bool| {
        let mut o = json!({"path": path, "uid": p.uid, "match": p.match_id, "name": p.name, "type": p.value.kind_name(),
            "value": p.value_at(l.layer_time(ctx.time)).to_json(), "enabled": true, "effectiveEnabled": effective_enabled});
        if !p.keys.is_empty() {
            o["keys"] = json!(
                p.keys
                    .iter()
                    .map(|k| json!({"time": k.time.seconds(), "value": k.value.to_json(), "in": k.in_interp.label(), "out": k.out_interp.label()}))
                    .collect::<Vec<_>>()
            );
        }
        expression_readback(s, ctx, l, p, &mut o);
        o
    };
    group(&l.props, "", 0, true, &sample, max_depth)
}

fn layer_tree(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid) = layer_p(s, p, "layer.tree")?;
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let idx = c.index_of(lid).unwrap_or(0);
    let l = c.layer(lid).ok_or(EngineError::NoComp)?;
    let depth = p.get("depth").and_then(Value::as_u64).unwrap_or(16).min(MAX_TREE_DEPTH as u64) as usize;
    let t = p.get("time").and_then(Value::as_f64).map(effectcraft_time::Tick::from_seconds_f64).unwrap_or_else(|| s.time_of(cid));
    let mut ctx = crate::render::EvalCtx::new(&s.project, cid, c, t);
    ctx.expr = s.expr.as_deref();
    let mut o = layer_json(l, idx);
    o["comp"] = json!(cid.0);
    o["revision"] = json!(s.revision);
    o["time"] = json!(t.seconds());
    o["timeBase"] = json!("comp");
    o["properties"] = group_json(s, &ctx, l, depth);
    Ok(o)
}

/// One command in full, with the JSON Schema of its parameters.
fn describe(s: &mut Session, p: &Value) -> Result<Value> {
    let id = str_p(p, "command").or(str_p(p, "id")).ok_or_else(|| super::bad("command.describe", "missing `command`"))?;
    let c = super::find(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
    let en = (c.enabled)(s);
    Ok(json!({
        "id": c.id, "label": c.label, "menu": c.menu, "shortcut": c.shortcut, "params": c.params,
        "schema": super::params_schema(c), "undoable": c.journal, "enabled": en.is_ok(), "why": en.err(),
        "atomicBatchSupported": super::batch_policy::atomic_supported(c.id),
    }))
}

/// What this build can do: counts agents use to orient themselves, and the parity summary.
fn capabilities(s: &mut Session, _: &Value) -> Result<Value> {
    let fx = effectcraft_effects::registry();
    let formats: Vec<Value> = s.exporter.as_ref().map(|e| e.formats().iter().map(|f| json!(format!("{f:?}"))).collect()).unwrap_or_default();
    let mut out = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "build": crate::build_info::build(),
        "protocolFeatures": crate::build_info::engine_protocol_features(),
        "commands": super::command_specs().len(),
        "effects": {"total": fx.len(), "gpu": fx.iter().filter(|e| e.gpu).count(), "float": fx.iter().filter(|e| e.float).count()},
        "export": formats,
        "import": s.importer.is_some(),
        "expressions": s.expr.is_some(),
        "scripting": s.script.is_some(),
        "gpu": s.accel.is_some(),
        "parity": {
            "target": "Adobe After Effects 2026",
            "summary": PARITY,
            "doc": "docs/target-app-parity.md",
        },
    });
    crate::build_info::session_identity(s, &mut out);
    Ok(out)
}

/// The headline of `docs/target-app-parity.md` (kept in sync by hand at each re-measure).
const PARITY: &str = "≈91% feature breadth and ≈45% ready for real work (estimated parity with After Effects 2026 26.5, stage alpha); all After Effects effects present (307 registered)";

fn state(s: &mut Session, _: &Value) -> Result<Value> {
    let mut v = serde_json::to_value(&s.state).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("revision".into(), json!(s.revision));
        // Resolve in the host process; a remote agent's working directory may differ.
        let project_path = s
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .and_then(|path| std::path::absolute(path).ok())
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path).to_string_lossy().into_owned());
        o.insert("projectPath".into(), json!(project_path));
        crate::agents::state_extras(s, o);
    }
    crate::build_info::session_identity(s, &mut v);
    if let Some(o) = v.as_object_mut() {
        o.insert("build".into(), crate::build_info::build());
        o.insert("protocolFeatures".into(), crate::build_info::engine_protocol_features());
    }
    Ok(v)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "command.list",
            "List Commands",
            "{filter?, enabledOnly?, schemas? (add each command's params JSON Schema), brief?: bool (only id + label; command.describe has the rest)}",
            list
        ),
        query!("command.describe", "Describe Command", "{command} → id, label, menu, shortcut, params doc, JSON `schema`, enabled", describe),
        query!("app.capabilities", "App Capabilities", "{} → version, command/effect counts, export formats, parity summary", capabilities),
        query!("project.summary", "Project Summary", "{}", summary),
        query!("comp.info", "Composition Info", "{comp?}", comp_info),
        query!("layer.tree", "Layer Property Tree", "{layer?, comp?, depth?, time? (comp s)} → nodes with `path`", layer_tree),
        query!(
            "editor.state",
            "Editor State",
            "{} → the editor state (yours when you pass `client`: your active comp, times, selection), revision, clients (other agents' cursors), claims",
            state
        ),
    ]
}

#[cfg(test)]
mod inspection_tests {
    use std::sync::Arc;

    use effectcraft_keyframe::{Keyframe, Value as KValue};
    use effectcraft_project::{Expression, ItemId, LayerId};
    use effectcraft_time::Tick;

    use super::*;

    fn fixture() -> (Session, ItemId, LayerId) {
        let mut s = Session::default();
        let comp = s.execute("comp.new", json!({"name": "Target", "duration": 10})).unwrap()["comp"].as_u64().unwrap();
        let layer = s.execute("layer.newSolid", json!({"name": "Subject"})).unwrap()["layer"].as_u64().unwrap();
        (s, ItemId(comp), LayerId(layer))
    }

    fn node<'a>(tree: &'a Value, path: &str) -> &'a Value {
        let mut stack = vec![&tree["properties"]];
        while let Some(n) = stack.pop() {
            if n["path"] == path {
                return n;
            }
            if let Some(children) = n["children"].as_array() {
                stack.extend(children);
            }
        }
        panic!("missing inspection path {path}");
    }

    #[test]
    fn command_description_reports_atomic_batch_support() {
        let mut s = Session::default();
        for (command, supported) in [("prop.get", true), ("layer.newShape", true), ("file.save", false), ("file.runScript", false)] {
            let info = s.execute("command.describe", json!({"command": command})).unwrap();
            assert_eq!(info["atomicBatchSupported"], supported, "{command}: {info}");
        }
    }

    #[test]
    fn editor_state_resolves_relative_project_paths_in_the_host() {
        let mut s = Session { path: Some("Cargo.toml".into()), ..Default::default() };
        let original = s.path.clone();
        let result = s.execute("editor.state", json!({})).unwrap();
        assert!(std::path::Path::new(result["projectPath"].as_str().unwrap()).is_absolute());
        assert_eq!(result["projectPath"], json!(std::fs::canonicalize("Cargo.toml").unwrap().to_string_lossy()));
        assert_eq!(s.path, original);
        assert_eq!(s.revision, 0);
    }

    #[test]
    fn editor_state_keeps_unsaved_paths_null_and_missing_paths_absolute() {
        let mut s = Session::default();
        assert!(s.execute("editor.state", json!({})).unwrap()["projectPath"].is_null());
        let name = format!("not-on-disk-host-relative-{}.ecproj", std::process::id());
        s.path = Some(name.clone());
        let result = s.execute("editor.state", json!({})).unwrap();
        assert_eq!(result["projectPath"], json!(std::path::absolute(&name).unwrap().to_string_lossy()));
        assert_eq!(s.path.as_deref(), Some(name.as_str()));
        assert_eq!(s.revision, 0);
    }

    #[test]
    fn target_comp_time_and_explicit_time_are_echoed_with_revision() {
        let (mut s, cid, lid) = fixture();
        let layer = Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(lid).unwrap();
        layer.start_time = Tick::from_seconds_f64(1.0);
        layer.stretch = 200.0;
        layer.props.prop_mut("transform/opacity").unwrap().keys =
            vec![Keyframe::new(Tick::ZERO, KValue::Scalar(10.0)), Keyframe::new(Tick::from_seconds_f64(1.0), KValue::Scalar(30.0))];
        s.state.times.insert(cid, Tick::from_seconds_f64(3.0));
        let active = s.execute("comp.new", json!({"name": "Active", "duration": 10})).unwrap()["comp"].as_u64().unwrap();
        s.state.times.insert(ItemId(active), Tick::from_seconds_f64(8.0));
        let revision = s.revision;
        let tree = s.execute("layer.tree", json!({"comp": cid.0, "layer": lid.0})).unwrap();
        assert_eq!(tree["comp"], cid.0);
        assert_eq!(tree["revision"], revision);
        assert_eq!(tree["time"], 3.0);
        assert_eq!(tree["timeBase"], "comp");
        assert_eq!(node(&tree, "transform/opacity")["value"], 30.0);
        assert_eq!(node(&tree, "transform/opacity")["rawValue"], 30.0);
        let info = s.execute("comp.info", json!({"comp": cid.0})).unwrap();
        assert_eq!(info["comp"], cid.0);
        assert_eq!(info["time"], 3.0);
        assert_eq!(info["revision"], revision);
        assert_eq!(info["timeBase"], "comp");
        let tree = s.execute("layer.tree", json!({"comp": cid.0, "layer": lid.0, "time": 1.0})).unwrap();
        assert_eq!(tree["time"], 1.0);
        assert_eq!(node(&tree, "transform/opacity")["value"], 10.0);
        assert_eq!(s.revision, revision, "inspection cannot edit the project");
    }

    struct ExpressionProbe;

    impl crate::render::ExprHost for ExpressionProbe {
        fn eval(&self, ctx: &crate::render::EvalCtx<'_>, _: &Layer, pr: &Property, raw: &KValue) -> std::result::Result<KValue, String> {
            if pr.expr.as_ref().is_some_and(|e| e.text == "error") {
                Err("expression failed".into())
            } else {
                Ok(KValue::Scalar(raw.as_f64() + ctx.time.seconds()))
            }
        }
    }

    #[test]
    fn expressions_keep_raw_values_and_report_enabled_disabled_and_error_samples() {
        let (mut s, cid, lid) = fixture();
        s.expr = Some(Arc::new(ExpressionProbe));
        s.expr_check = Some(|text| if text == "syntax" { Err("syntax error".into()) } else { Ok(()) });
        s.state.times.insert(cid, Tick::from_seconds_f64(2.0));
        let uid = s.project.comp(cid).unwrap().layer(lid).unwrap().props.prop("transform/opacity").unwrap().uid;
        for (text, enabled, expected, error) in [
            ("time", true, 12.0, None),
            ("time", false, 10.0, None),
            ("error", true, 10.0, Some("expression failed")),
            ("syntax", false, 10.0, Some("syntax error")),
            ("", true, 10.0, None),
        ] {
            let pr = Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(lid).unwrap().props.find_mut(uid).unwrap();
            pr.value = KValue::Scalar(10.0);
            pr.expr = Some(Expression { text: text.into(), enabled });
            let tree = s.execute("layer.tree", json!({"comp": cid.0, "layer": lid.0})).unwrap();
            let sample = node(&tree, "transform/opacity");
            assert_eq!(sample["uid"], uid);
            assert_eq!(sample["value"], 10.0);
            assert_eq!(sample["rawValue"], 10.0);
            assert_eq!(sample["evaluated"], expected);
            assert_eq!(sample["evaluatedValue"], expected);
            assert_eq!(sample["expression"], text);
            assert_eq!(sample["expressionEnabled"], enabled);
            assert_eq!(sample["expressionEffectiveEnabled"], enabled && !text.is_empty());
            assert_eq!(sample.get("expressionError").and_then(Value::as_str), error);
            let read = s.execute("prop.get", json!({"comp": cid.0, "layer": lid.0, "prop": uid})).unwrap();
            assert_eq!(sample["value"], read["value"]);
            // The legacy property read already reports evaluated samples for nonempty expressions.
            if !text.is_empty() {
                assert_eq!(sample["evaluated"], read["evaluated"]);
            }
        }
        s.expr = None;
        Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(lid).unwrap().props.find_mut(uid).unwrap().expr =
            Some(Expression { text: "time".into(), enabled: true });
        let tree = s.execute("layer.tree", json!({"comp": cid.0, "layer": lid.0})).unwrap();
        let sample = node(&tree, "transform/opacity");
        assert_eq!(sample["evaluatedValue"], 10.0);
        assert_eq!(sample["expressionError"], "expression engine unavailable");
    }

    #[test]
    fn disabled_effect_parents_preserve_local_switches_uids_and_expression_state() {
        let (mut s, cid, lid) = fixture();
        s.expr = Some(Arc::new(ExpressionProbe));
        let mut pr = Property::new(90_003, "amount", "Amount", KValue::Scalar(4.0));
        pr.expr = Some(Expression { text: "time".into(), enabled: true });
        let mut effect = PropGroup::new(90_002, "sample.effect", "Sample Effect").with(pr);
        effect.kind = GroupKind::Effect { effect: "sample.effect".into() };
        let mut disabled = PropGroup::new(90_001, "disabled", "Disabled parent").with(effect);
        disabled.enabled = false;
        disabled.kind = GroupKind::Effect { effect: "sample.parent".into() };
        Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(lid).unwrap().props.children.push(Node::Group(disabled));
        let tree = s.execute("layer.tree", json!({"comp": cid.0, "layer": lid.0})).unwrap();
        let parent = node(&tree, "disabled");
        assert_eq!(parent["uid"], 90_001);
        assert_eq!(parent["enabled"], false);
        assert_eq!(parent["effectiveEnabled"], false);
        let effect = node(&tree, "disabled/#1");
        assert_eq!(effect["uid"], 90_002);
        assert_eq!(effect["enabled"], true);
        assert_eq!(effect["effectiveEnabled"], false);
        let sample = node(&tree, "disabled/#1/amount");
        assert_eq!(sample["uid"], 90_003);
        assert_eq!(sample["enabled"], true);
        assert_eq!(sample["effectiveEnabled"], false);
        assert_eq!(sample["expressionEnabled"], true);
        assert_eq!(sample["expressionEffectiveEnabled"], false);
        assert_eq!(sample["evaluatedValue"], 4.0);
        let props = &s.project.comp(cid).unwrap().layer(lid).unwrap().props;
        assert_eq!(property_effective_enabled(props, 90_003), Some(false));
        assert_eq!(property_effective_enabled(props, u64::MAX), None);
    }
}
