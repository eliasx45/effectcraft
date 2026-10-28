//! Validate name aliases before command execution, including callers that use Option resolvers.

use effectcraft_project::props::Seg;
use effectcraft_project::{Comp, ItemId, LayerId, Node, PropGroup, parse_path};
use serde_json::{Map, Value, json};

use super::{bad, comp_id, pick_group, resolve_layer};
use crate::{Result, Session};

pub(crate) fn checked_layer_ref(comp: &Comp, cid: ItemId, value: &Value, cmd: &str) -> Result<Option<LayerId>> {
    if let Some(name) = value.as_str().filter(|s| !s.starts_with('#')) {
        let candidates: Vec<_> = comp.layers.iter().filter(|l| l.name == name).map(|l| json!({"id": l.id.0, "name": l.name, "comp": cid.0})).collect();
        if candidates.len() > 1 {
            return Err(bad(
                cmd,
                format!("ambiguous layer name {name:?} in comp {}; candidates: {}; use a numeric layer id or #n", cid.0, Value::Array(candidates)),
            ));
        }
    }
    Ok(resolve_layer(comp, value))
}

fn group_ref<'a>(groups: impl Iterator<Item = &'a PropGroup>, value: &Value, cmd: &str, context: &str) -> Result<Option<&'a PropGroup>> {
    let groups: Vec<_> = groups.collect();
    if let Some(name) = value.as_str().filter(|s| !s.starts_with(['#', '@'])) {
        let candidates: Vec<_> =
            groups.iter().filter(|g| g.name == name || g.match_id == name).map(|g| json!({"uid": g.uid, "name": g.name, "match": g.match_id})).collect();
        if candidates.len() > 1 {
            return Err(bad(
                cmd,
                format!("ambiguous group/effect name {name:?} in {context}; candidates: {}; use a numeric uid, @uid or #n", Value::Array(candidates)),
            ));
        }
    }
    Ok(pick_group(groups.into_iter(), value))
}

/// Paths emitted by layer.tree use match ids (including the first unsuffixed occurrence).
/// Display-name aliases must be unique; explicit #n, match#n and @uid keep their meaning.
pub(crate) fn validate_path(root: &PropGroup, path: &str, cmd: &str, context: &str) -> Result<Option<u64>> {
    if path.len() > 65_536 || path.split(['/', '.']).filter(|s| !s.is_empty()).take(257).count() > 256 {
        return Err(bad(cmd, format!("property path exceeds 64 KiB or 256 segments in {context}")));
    }
    if let Some(uid) = path.trim().strip_prefix('@').and_then(|u| u.parse::<u64>().ok()) {
        // The command checks existence; an explicit uid needs no name-resolution tree walk.
        return Ok(Some(uid));
    }
    let raw: Vec<_> = path.split(['/', '.']).filter(|s| !s.is_empty()).collect();
    let mut group = root;
    let mut last_uid = None;
    for (i, (seg, raw_seg)) in parse_path(path).iter().zip(&raw).enumerate() {
        let node = match seg {
            Seg::Uid(uid) => group.children.iter().find(|n| n.uid() == *uid),
            Seg::Index(n) => n.checked_sub(1).and_then(|i| group.children.get(i)),
            Seg::Match(name, occurrence) => {
                let matches: Vec<_> = group.children.iter().filter(|n| n.match_id() == name || n.name().eq_ignore_ascii_case(name)).collect();
                let explicit_occurrence = raw_seg.rsplit_once('#').is_some_and(|(_, n)| n.parse::<usize>().is_ok());
                let canonical = matches.iter().any(|n| n.match_id() == name);
                if matches.len() > 1 && !explicit_occurrence && !canonical {
                    let candidates: Vec<_> = matches.iter().map(|n| json!({"uid": n.uid(), "name": n.name(), "match": n.match_id()})).collect();
                    return Err(bad(
                        cmd,
                        format!(
                            "ambiguous property/group name {name:?} in {context}, parent @{} ({:?}); candidates: {}; use @uid or an explicit occurrence",
                            group.uid,
                            group.name,
                            Value::Array(candidates)
                        ),
                    ));
                }
                occurrence.checked_sub(1).and_then(|i| matches.get(i).copied())
            }
        };
        let Some(node) = node else { return Ok(None) };
        last_uid = Some(node.uid());
        if i.saturating_add(1) < raw.len() {
            let Node::Group(child) = node else { return Ok(None) };
            group = child;
        }
    }
    Ok(last_uid)
}

fn validate_layer_fields(s: &Session, cmd: &str, p: &Value, cid: ItemId, comp: &Comp, out: &mut Map<String, Value>) -> Result<Vec<LayerId>> {
    let mut targets = vec![];
    for key in ["layers", "layer"] {
        if let Some(value) = p.get(key) {
            let refs: Vec<_> = match value {
                Value::Array(a) => a.iter().collect(),
                _ => vec![value],
            };
            let mut ids = vec![];
            for value in refs {
                if let Some(id) = checked_layer_ref(comp, cid, value, cmd)? {
                    ids.push(id);
                }
            }
            out.insert(
                key.into(),
                if value.is_array() { json!(ids.iter().map(|l| l.0).collect::<Vec<_>>()) } else { ids.first().map_or(Value::Null, |l| json!(l.0)) },
            );
            targets.extend(ids);
        }
    }
    // Secondary references are command-specific: `parent`, `source` and `target` also name
    // project folders, import sources and nested property objects on unrelated commands.
    let secondary: &[&str] = match cmd {
        "layer.setParent" => &["parent"],
        "layer.setTrackMatte" => &["matte"],
        "layer.arrange" => &["above"],
        "track.property" => &["source"],
        "track.setTarget" | "track.new" | "track.motion" | "track.stabilize" => &["target"],
        _ if cmd.starts_with("markers.") => &["toLayer"],
        _ if cmd.starts_with("contentFill.") || cmd == "layer.newContentAwareFill" => &["referenceLayer"],
        _ => &[],
    };
    for key in secondary.iter().copied().chain(["camera"]) {
        if let Some(value) = p.get(key).filter(|v| !v.is_null())
            && let Some(id) = checked_layer_ref(comp, cid, value, cmd)?
        {
            out.insert(key.into(), json!(id.0));
        }
    }
    if targets.is_empty() && p.get("layer").is_none() && p.get("layers").is_none() {
        targets.extend(s.state.selected_layers.iter().copied());
        targets.extend(s.state.selected_props.iter().map(|(l, _)| *l));
        targets.extend(s.state.selected_keys.iter().map(|k| k.layer));
        if cmd.starts_with("track.")
            && let Some((l, _)) = s.state.current_track
        {
            targets.push(l);
        }
        if ["roto.", "objectMatte.", "warp.", "camera."].iter().any(|prefix| cmd.starts_with(prefix)) {
            targets.extend(comp.layers.iter().map(|l| l.id));
        }
    }
    targets.sort_by_key(|l| l.0);
    targets.dedup();
    Ok(targets)
}

fn validate_object(s: &Session, cmd: &str, p: &Value, inherited: Option<ItemId>, depth: usize) -> Result<Map<String, Value>> {
    if depth > 64 {
        return Err(bad(cmd, "target references exceed the maximum nesting depth (64)"));
    }
    let mut out = Map::new();
    let cid = if p.get("comp").is_some_and(|v| !v.is_null()) { Some(comp_id(s, p)?) } else { inherited.or_else(|| s.active_comp_id()) };
    let Some(cid) = cid else { return Ok(out) };
    let Some(comp) = s.project.comp(cid) else { return Ok(out) };
    let targets = validate_layer_fields(s, cmd, p, cid, comp, &mut out)?;
    if !out.is_empty() || p.get("comp").is_some() {
        out.insert("comp".into(), json!(cid.0));
    }
    let mut nodes = vec![];
    for lid in targets {
        let Some(layer) = comp.layer(lid) else { continue };
        let context = format!("comp {}, layer {} ({:?})", cid.0, lid.0, layer.name);
        for (key, root) in [("effect", layer.effects()), ("mask", layer.masks()), ("animator", layer.props.group("text/animators"))] {
            if cmd == "effect.apply" {
                continue;
            }
            if let (Some(value), Some(root)) = (p.get(key), root)
                && let Some(g) = group_ref(root.groups(), value, cmd, &context)?
            {
                nodes.push(json!({"kind": key, "layer": lid.0, "uid": g.uid}));
                if key == "effect"
                    && let Some(path) = p.get("param").and_then(Value::as_str)
                {
                    validate_path(g, path, cmd, &context)?;
                }
            }
        }
        if let Some(value) = p.get("tracker") {
            group_ref(layer.trackers().map(|(g, _)| g), value, cmd, &context)?;
        }
        for key in ["path", "prop", "group"] {
            let Some(path) = p.get(key).and_then(Value::as_str) else { continue };
            // `path` is a filesystem location in import/export and plug-in commands.
            let property_path = key == "prop"
                || (key == "group" && cmd == "layer.addShapeItem")
                || (key == "path"
                    && (p.get("prop").is_none() || p.get("prop").is_some_and(Value::is_string))
                    && (["prop.", "keys.", "paths."].iter().any(|prefix| cmd.starts_with(prefix))
                        || matches!(
                            cmd,
                            "essential.addProperty" | "essential.linkProperty" | "essential.unlinkProperty" | "essential.canAdd" | "effect.editDropdown"
                        )));
            if property_path && let Some(uid) = validate_path(&layer.props, path, cmd, &context)? {
                nodes.push(json!({"kind": key, "layer": lid.0, "uid": uid}));
            }
        }
        if let Some(uid) = p.get("prop").and_then(Value::as_u64) {
            nodes.push(json!({"kind": "prop", "layer": lid.0, "uid": uid}));
        }
    }
    if !nodes.is_empty() {
        out.insert("nodes".into(), Value::Array(nodes));
        out.insert("comp".into(), json!(cid.0));
    }
    for key in ["keys", "vertices"] {
        if let Some(entries) = p.get(key).and_then(Value::as_array) {
            for entry in entries {
                validate_object(s, cmd, entry, Some(cid), depth.saturating_add(1))?;
            }
        }
    }
    if cmd == "prop.pickWhip"
        && let Some(target) = p.get("target")
    {
        validate_object(s, cmd, target, None, depth.saturating_add(1))?;
    }
    Ok(out)
}

/// Called before enablement/run. The returned identities were resolved against the document
/// before the write (so deleting or renaming its target cannot change the confirmation).
pub(crate) fn validate_target_refs(s: &Session, cmd: &str, p: &Value) -> Result<Value> {
    // Render requests with `item` resolve their real comp inside request(), which also uses
    // checked_layer_ref. The active comp is not the parent of these references.
    if cmd.starts_with("render.") && p.get("item").is_some_and(|v| !v.is_null()) {
        return Ok(json!({}));
    }
    // memory.purge takes a comp id that may be gone from the project (its buffers still cached)
    // and resolves it itself.
    if cmd == "memory.purge" {
        return Ok(json!({}));
    }
    // comp.open accepts `item` as an alias; its other caller resolves through comp_id too.
    if cmd == "comp.open"
        && p.get("comp").is_none()
        && let Some(item) = p.get("item")
    {
        return validate_object(s, cmd, &json!({"comp": item}), None, 0).map(Value::Object);
    }
    validate_object(s, cmd, p, None, 0).map(Value::Object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_project::GroupKind;

    fn session() -> Session {
        let mut s = Session::default();
        s.execute("comp.new", json!({"name": "Same", "width": 100, "height": 100, "duration": 1})).unwrap();
        s
    }

    fn layer(s: &mut Session, name: &str) -> u64 {
        let lid = s.execute("layer.newSolid", json!({"name": name})).unwrap()["layer"].as_u64().unwrap();
        let cid = s.active_comp_id().unwrap();
        // Simulate imported/renamed duplicate layers; the builder uniquifies new names.
        std::sync::Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(LayerId(lid)).unwrap().name = name.into();
        lid
    }

    fn assert_no_edit(s: &mut Session, cmd: &str, p: Value, expected: &str) -> String {
        let before = s.project.to_json();
        let revision = s.revision;
        let journal = s.journal.len();
        let state = serde_json::to_value(&s.state).unwrap();
        let err = s.execute(cmd, p).unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, revision);
        assert_eq!(s.journal.len(), journal);
        assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
        err
    }

    #[test]
    fn ambiguous_comps_fail_with_ids_and_parent_before_mutation() {
        let mut s = session();
        let first = s.active_comp_id().unwrap().0;
        s.execute("comp.new", json!({"name": "Same", "width": 100, "height": 100, "duration": 1})).unwrap();
        let second = s.active_comp_id().unwrap().0;
        // Imported projects can contain duplicate names; comp.new intentionally uniquifies.
        std::sync::Arc::make_mut(&mut s.project).item_mut(ItemId(second)).unwrap().name = "Same".into();
        let err = assert_no_edit(&mut s, "comp.settings", json!({"comp": "Same", "width": 200}), "ambiguous composition");
        assert!(err.contains(&first.to_string()) && err.contains(&second.to_string()) && err.contains("parent"));
        assert_no_edit(&mut s, "comp.open", json!({"item": "Same"}), "ambiguous composition");
        s.execute("comp.settings", json!({"comp": first, "width": 200})).unwrap();
        assert_eq!(s.project.comp(ItemId(first)).unwrap().width, 200);
    }

    #[test]
    fn ambiguous_layers_fail_with_ids_and_comp_before_mutation() {
        let mut s = session();
        let first = layer(&mut s, "Twin");
        let second = layer(&mut s, "Twin");
        let err = assert_no_edit(&mut s, "layer.rename", json!({"layer": "Twin", "name": "Changed"}), "ambiguous layer");
        assert!(err.contains(&first.to_string()) && err.contains(&second.to_string()) && err.contains("comp"));
        s.execute("layer.rename", json!({"layer": first, "name": "Changed"})).unwrap();
        assert_eq!(s.active_comp().unwrap().layer(LayerId(first)).unwrap().name, "Changed");
        assert_eq!(s.active_comp().unwrap().layer(LayerId(second)).unwrap().name, "Twin");
    }

    #[test]
    fn ambiguous_array_secondary_and_nested_layer_references_fail() {
        let mut s = session();
        let first = layer(&mut s, "Twin");
        layer(&mut s, "Twin");
        for (cmd, p) in [
            ("layer.select", json!({"layers": [first, "Twin"]})),
            ("layer.setParent", json!({"layer": first, "parent": "Twin"})),
            ("layer.setTrackMatte", json!({"layer": first, "matte": "Twin"})),
            ("keys.select", json!({"keys": [{"layer": "Twin", "path": "transform/opacity", "time": 0}]})),
            ("mask.selectVertices", json!({"vertices": [{"layer": "Twin", "mask": 1, "index": 0}]})),
        ] {
            assert_no_edit(&mut s, cmd, p, "ambiguous layer");
        }
    }

    #[test]
    fn ambiguous_effects_fail_and_uid_or_position_remains_usable() {
        let mut s = session();
        let lid = layer(&mut s, "Plate");
        let cid = s.active_comp_id().unwrap();
        let fx = std::sync::Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(LayerId(lid)).unwrap().props.sub_mut("effects").unwrap();
        for uid in [90001, 90002] {
            let mut group = PropGroup::new(uid, "duplicate.effect", "Twin FX");
            group.kind = GroupKind::Effect { effect: "duplicate.effect".into() };
            fx.children.push(Node::Group(group));
        }
        let err = assert_no_edit(&mut s, "effect.toggle", json!({"layer": lid, "effect": "Twin FX"}), "ambiguous group/effect");
        assert!(err.contains("90001") && err.contains("90002") && err.contains(&format!("layer {lid}")));
        assert_no_edit(&mut s, "effect.toggle", json!({"layer": lid, "effect": "duplicate.effect"}), "ambiguous group/effect");
        assert_no_edit(&mut s, "effect.setLabel", json!({"layer": lid, "effect": "Twin FX", "label": "Red"}), "ambiguous group/effect");
        s.state.selected_layers.clear();
        s.state.selected_props.clear();
        assert_no_edit(&mut s, "objectMatte.unfreeze", json!({"effect": "Twin FX"}), "ambiguous group/effect");
        for value in [json!(90001), json!("@90001"), json!("#1")] {
            s.execute("effect.toggle", json!({"layer": lid, "effect": value})).unwrap();
        }
    }

    #[test]
    fn applying_a_catalog_effect_again_does_not_resolve_existing_instances() {
        let mut s = session();
        let lid = layer(&mut s, "Plate");
        let mut id = String::new();
        for _ in 0..3 {
            let effect = if id.is_empty() { "Gaussian Blur" } else { &id };
            let result = s.execute("effect.apply", json!({"layer": lid, "effect": effect})).unwrap();
            id = result["effect"].as_str().unwrap().to_string();
        }
        assert_eq!(s.active_comp().unwrap().layer(LayerId(lid)).unwrap().effects().unwrap().groups().count(), 3);
    }

    #[test]
    fn duplicate_display_aliases_fail_but_canonical_paths_keep_their_meaning() {
        let root = PropGroup::new(1, "root", "Root").with(PropGroup::new(2, "group", "Twin")).with(PropGroup::new(3, "group", "Twin"));
        let err = validate_path(&root, "Twin", "prop.set", "comp 1, layer 4").unwrap_err().to_string();
        assert!(err.contains("ambiguous") && err.contains("parent @1") && err.contains("2") && err.contains("3"));
        for (path, uid) in [("group", 2), ("group#1", 2), ("group#2", 3), ("#2", 3), ("@3", 3), ("Twin#2", 3)] {
            assert_eq!(validate_path(&root, path, "prop.set", "comp 1, layer 4").unwrap(), Some(uid));
        }
    }

    #[test]
    fn duplicate_nested_property_group_names_fail_before_write() {
        let mut s = session();
        let lid = layer(&mut s, "Plate");
        let cid = s.active_comp_id().unwrap();
        let root = &mut std::sync::Arc::make_mut(&mut s.project).comp_mut(cid).unwrap().layer_mut(LayerId(lid)).unwrap().props;
        root.children.push(Node::Group(PropGroup::new(90001, "a", "Twin")));
        root.children.push(Node::Group(PropGroup::new(90002, "b", "Twin")));
        assert_no_edit(&mut s, "prop.set", json!({"layer": lid, "path": "Twin/value", "value": 5}), "ambiguous property/group");
        assert_no_edit(
            &mut s,
            "prop.pickWhip",
            json!({"layer": lid, "path": "transform/opacity", "target": {"layer": lid, "path": "Twin/value"}}),
            "ambiguous property/group",
        );
    }

    #[test]
    fn identities_are_captured_before_rename_and_uid_targets_work() {
        let mut s = session();
        let lid = layer(&mut s, "Plate");
        let uid = s.active_comp().unwrap().layer(LayerId(lid)).unwrap().props.prop("transform/opacity").unwrap().uid;
        let resolved = validate_target_refs(&s, "prop.set", &json!({"layer": "Plate", "path": "transform/opacity", "value": 50})).unwrap();
        assert_eq!(resolved["layer"], lid);
        assert_eq!(resolved["nodes"][0]["uid"], uid);
        s.execute("prop.set", json!({"layer": lid, "path": format!("@{uid}"), "value": 50})).unwrap();
        assert_eq!(s.active_comp().unwrap().layer(LayerId(lid)).unwrap().props.find(uid).unwrap().value.as_f64(), 50.0);
    }

    #[test]
    fn render_requests_use_the_explicit_item_as_layer_parent() {
        let mut s = session();
        let duplicate_comp = s.active_comp_id().unwrap().0;
        layer(&mut s, "Twin");
        layer(&mut s, "Twin");
        s.execute("comp.new", json!({"name": "Other", "width": 100, "height": 100, "duration": 1})).unwrap();
        let other = s.active_comp_id().unwrap().0;
        let single = layer(&mut s, "Twin");
        let p = json!({"item": duplicate_comp, "layer": "Twin"});
        assert!(crate::render_view::request(&s, &p, "render.frame").err().unwrap().to_string().contains("ambiguous layer"));
        s.execute("comp.open", json!({"comp": duplicate_comp})).unwrap();
        let p = json!({"item": other, "layer": "Twin"});
        assert!(validate_target_refs(&s, "render.frame", &p).unwrap().as_object().unwrap().is_empty());
        assert_eq!(crate::render_view::request(&s, &p, "render.frame").unwrap().layer, Some(LayerId(single)));
        s.execute("comp.new", json!({"name": "Same", "width": 100, "height": 100, "duration": 1})).unwrap();
        let duplicate_item = s.active_comp_id().unwrap();
        std::sync::Arc::make_mut(&mut s.project).item_mut(duplicate_item).unwrap().name = "Same".into();
        assert!(crate::render_view::request(&s, &json!({"item": "Same"}), "render.frame").err().unwrap().to_string().contains("ambiguous project item"));
    }

    #[test]
    fn adversarial_nested_references_return_an_error() {
        let s = session();
        let mut p = json!({});
        for _ in 0..66 {
            p = json!({"keys": [p]});
        }
        assert!(validate_target_refs(&s, "keys.select", &p).unwrap_err().to_string().contains("nesting depth"));
        let root = PropGroup::new(1, "root", "Root");
        assert!(validate_path(&root, &"a".repeat(65_537), "prop.set", "layer 1").is_err());
        assert!(validate_path(&root, &vec!["a"; 257].join("/"), "prop.set", "layer 1").is_err());
    }
}
