//! Edit a nested composition through an instance path without changing the parent viewer.
use std::collections::BTreeSet;

use effectcraft_project::{ItemId, LayerId, LayerSource};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, str_p};
use crate::{EngineError, Result, Session, cmd};

pub const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug)]
pub struct Context {
    pub comp: ItemId,
    pub time: Tick,
    /// Parent time = offset + nested-comp time * scale. Nonlinear instances have no unique inverse.
    pub affine: Option<(f64, f64)>,
    pub locked: bool,
}

pub fn context(s: &Session, root: ItemId, path: &[u64], time: Tick) -> Result<Context> {
    if path.is_empty() || path.len() > MAX_DEPTH {
        return Err(bad("comp.inlineEdit", "instance path must contain 1–32 precomp layers"));
    }
    let mut result = Context { comp: root, time, affine: Some((0.0, 1.0)), locked: false };
    let mut seen = BTreeSet::from([root]);
    for id in path {
        let comp = s.project.comp(result.comp).ok_or(EngineError::NoComp)?;
        let layer = comp.layer(LayerId(*id)).ok_or_else(|| bad("comp.inlineEdit", "instance layer no longer exists"))?;
        let LayerSource::Comp { item } = layer.source else { return Err(bad("comp.inlineEdit", "instance path must contain only precomps")) };
        if !seen.insert(item) {
            return Err(bad("comp.inlineEdit", "cyclic precomp cannot expand inline"));
        }
        if !layer.stretch.is_finite() || layer.stretch.abs() < 1e-9 {
            return Err(bad("comp.inlineEdit", "invalid precomp time stretch"));
        }
        let child = s.project.comp(item).ok_or(EngineError::NoComp)?;
        let mut eval = crate::render::EvalCtx::new(&s.project, result.comp, comp, result.time);
        eval.expr = s.expr.as_deref();
        result.time = eval.nested_time(layer);
        result.locked |= layer.switches.locked;
        result.affine = result.affine.and_then(|(offset, scale)| {
            if layer.props.get("timeRemap").is_some()
                || child.preserve_frame_rate
                || ((layer.stretch - 100.0).abs() > 1e-9 && child.markers.iter().any(|m| m.protected && m.duration > Tick::ZERO))
            {
                return None;
            }
            let next = (offset + scale * layer.start_time.seconds(), scale * layer.stretch / 100.0);
            (next.0.is_finite() && next.1.is_finite() && next.1.abs() > 1e-9).then_some(next)
        });
        result.comp = item;
    }
    Ok(result)
}

/// Synchronous source edits supported by the inline cursor (including its graph and clipboard).
pub fn supported(command: &str) -> bool {
    matches!(
        command,
        "edit.copy"
            | "edit.cut"
            | "edit.paste"
            | "edit.pasteSvg"
            | "edit.pasteFigCraft"
            | "edit.copyWithPropertyLinks"
            | "edit.copyWithRelativePropertyLinks"
            | "edit.copyExpressionOnly"
            | "edit.pasteReversedKeyframes"
            | "edit.selectAll"
            | "edit.deselectAll"
            | "keys.copy"
            | "keys.paste"
            | "keys.pasteGraph"
            | "keys.selectAll"
            | "keys.set"
            | "keys.setEase"
            | "keys.transform"
            | "keys.velocity"
            | "keys.interpolation"
            | "keys.toggleTransform"
            | "keys.setLabel"
            | "keys.timeReverse"
            | "prop.separateDimensions"
            | "prop.graphPin"
            | "edit.clear"
            | "edit.duplicate"
            | "layer.select"
            | "layer.rename"
            | "layer.setSwitch"
            | "layer.setBlendMode"
            | "layer.setParent"
            | "layer.setTrackMatte"
            | "layer.timing"
            | "layer.arrange"
            | "prop.set"
            | "prop.select"
            | "prop.deselect"
            | "prop.toggleAnimation"
            | "prop.toggleKey"
            | "prop.addKey"
            | "prop.setExpression"
            | "prop.setGroupEnabled"
            | "prop.renameGroup"
            | "prop.reset"
            | "keys.select"
            | "keys.move"
            | "keys.delete"
            | "keys.toggleHold"
            | "keys.easyEase"
            | "keys.easyEaseIn"
            | "keys.easyEaseOut"
    )
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let command = str_p(p, "command").ok_or_else(|| bad("comp.inlineEdit", "missing command"))?;
    if !supported(command) {
        return Err(bad("comp.inlineEdit", "this command is not supported for inline editing"));
    }
    let root = s.resolve_comp(p.get("root"))?;
    let path = p.get("path").and_then(Value::as_array).ok_or_else(|| bad("comp.inlineEdit", "missing instance path"))?;
    if path.len() > MAX_DEPTH {
        return Err(bad("comp.inlineEdit", "instance path is too deep"));
    }
    let path: Vec<u64> = path.iter().map(|v| v.as_u64().ok_or_else(|| bad("comp.inlineEdit", "invalid instance layer id"))).collect::<Result<_>>()?;
    let mut context = context(s, root, &path, s.time_of(root))?;
    if let Some(value) = p.get("sourceTime") {
        let seconds = value.as_f64().filter(|t| t.is_finite()).ok_or_else(|| bad("comp.inlineEdit", "sourceTime must be finite source-comp seconds"))?;
        context.time = Tick::from_seconds_f64(seconds);
    }
    if context.locked {
        return Err(bad("comp.inlineEdit", "a containing precomp layer is locked"));
    }
    let mut params = p.get("params").cloned().unwrap_or_else(|| json!({}));
    let obj = params.as_object_mut().ok_or_else(|| bad("comp.inlineEdit", "params must be an object"))?;
    if obj.contains_key("client") || obj.get("comp").is_some_and(|comp| comp.as_u64() != Some(context.comp.0)) {
        return Err(bad("comp.inlineEdit", "inline command targets are determined by the instance path"));
    }
    if command == "edit.pasteFigCraft" {
        if obj.get("target").and_then(Value::as_str).is_some_and(|target| !matches!(target, "auto" | "activeComp")) {
            return Err(bad("comp.inlineEdit", "inline paste must target the source composition"));
        }
        obj.insert("target".into(), json!("activeComp"));
    }
    obj.insert("comp".into(), json!(context.comp.0));
    if let Some(Value::String(merge)) = obj.get_mut("merge") {
        *merge = format!("inline-{}-{merge}", context.comp.0);
    }
    let previous = s.state.active_comp;
    let previous_time = s.state.times.get(&context.comp).copied();
    let track = s.state.current_track;
    let text_edit = s.state.text_edit.take();
    s.activate_comp(context.comp);
    s.state.times.insert(context.comp, context.time);
    let result = atomic_edit(s, command, params);
    match previous {
        Some(id) => s.activate_comp(id),
        None => s.activate_comp_none(),
    }
    match previous_time {
        Some(t) => {
            s.state.times.insert(context.comp, t);
        }
        None => {
            s.state.times.remove(&context.comp);
        }
    }
    s.state.current_track = track;
    s.state.text_edit = text_edit;
    result
}

/// Same rollback boundary as an atomic engine batch, without interpreting literal `$1` text
/// as batch references. The whitelist above excludes external side effects and async work.
fn atomic_edit(s: &mut Session, command: &str, params: Value) -> Result<Value> {
    let (project, history, state) = (s.project.clone(), s.history.clone(), s.state.clone());
    let (events, journal, learn) = (s.events.clone(), s.journal.clone(), s.learn.clone());
    let key_move = s.key_move.clone();
    let (warp_pending, camera_pending, roto_pending, analyses_checked) =
        (s.warp_pending.clone(), s.camera_pending.clone(), s.roto_pending.clone(), s.analyses_checked.clone());
    let merge = params.get("merge").and_then(Value::as_str).map(str::to_string);
    let merging = merge.is_some() && merge == history.merge_key && history.author(&project) == s.client.as_deref();
    let result = s.execute(command, params);
    if result.is_err() {
        s.project = project;
        s.history = history;
        s.state = state;
        s.events = events;
        s.journal = journal;
        s.learn = learn;
        s.key_move = key_move;
        s.warp_pending = warp_pending;
        s.camera_pending = camera_pending;
        s.roto_pending = roto_pending;
        s.analyses_checked = analyses_checked;
        s.sanitize_state();
        s.bump();
    } else {
        // Rebuild the single atomic history step from the saved history. Counting appended
        // entries fails when internal edits evict the first snapshot at the undo limit.
        s.history = history;
        if !std::sync::Arc::ptr_eq(&s.project, &project) {
            let label = super::find(command).map(|spec| spec.label).unwrap_or("Edit Nested Composition");
            if merging {
                s.history.abandon_redo(&project);
            } else {
                s.history.record(label, project, s.prefs.general.undo_levels.max(1) as usize);
            }
            s.history.merge_key = merge;
            s.history.set_author(&s.project, s.client.clone());
        }
    }
    result
}

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "comp.inlineEdit",
        "Edit Nested Composition Inline",
        [],
        None,
        "{root? (parent comp), path: [precomp layer ids], command, params?, sourceTime? (source-comp seconds, e.g. a dialog opened earlier)} → execute once in nested source time; preserve parent viewer and per-comp selections",
        always,
        run
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Session, ItemId, ItemId, u64, u64, u64) {
        let mut s = Session::default();
        let child = ItemId(s.execute("comp.new", json!({"name":"Source","duration":20,"frameRate":30})).unwrap()["comp"].as_u64().unwrap());
        let leaf = s.execute("layer.newSolid", json!({"name":"Shared","width":32,"height":32})).unwrap()["layer"].as_u64().unwrap();
        let root = ItemId(s.execute("comp.new", json!({"name":"Parent","duration":30,"frameRate":30})).unwrap()["comp"].as_u64().unwrap());
        let a = s.execute("layer.addItem", json!({"item":child.0})).unwrap()["layer"].as_u64().unwrap();
        let b = s.execute("layer.addItem", json!({"item":child.0})).unwrap()["layer"].as_u64().unwrap();
        (s, root, child, leaf, a, b)
    }

    fn inline(s: &mut Session, path: &[u64], command: &str, params: Value) -> Result<Value> {
        s.execute("comp.inlineEdit", json!({"path":path,"command":command,"params":params}))
    }

    #[test]
    fn compound_edits_keep_one_undo_at_history_capacity() {
        let (mut s, _, child, leaf, a, _) = fixture();
        s.prefs.general.undo_levels = 1;
        s.execute("prop.separateDimensions", json!({"comp":child.0,"layer":leaf,"value":true})).unwrap();
        let before = s.project.clone();
        assert_eq!(s.history.undo.len(), 1);
        inline(&mut s, &[a], "prop.set", json!({"layer":leaf,"path":"transform/position","value":[10,20]})).unwrap();
        assert_eq!(s.history.undo.len(), 1);
        assert!(s.undo());
        assert!(s.project.same_content(&before));
    }

    #[test]
    fn partial_separated_property_error_rolls_back_and_valid_edit_is_one_undo() {
        let (mut s, root, child, leaf, a, _) = fixture();
        s.execute("prop.separateDimensions", json!({"comp":child.0,"layer":leaf,"value":true})).unwrap();
        let before = s.project.clone();
        let undo = s.history.undo.len();
        assert!(inline(&mut s, &[a], "prop.set", json!({"layer":leaf,"path":"transform/position","value":[10,"invalid"]})).is_err());
        assert!(s.project.same_content(&before));
        assert_eq!(s.history.undo.len(), undo);
        assert_eq!(s.active_comp_id(), Some(root));
        inline(&mut s, &[a], "prop.set", json!({"layer":leaf,"path":"transform/position","value":[10,20]})).unwrap();
        assert_eq!(s.history.undo.len(), undo + 1);
        assert!(s.undo());
        assert!(s.project.same_content(&before));
        inline(&mut s, &[a], "layer.rename", json!({"layer":leaf,"name":"$1"})).unwrap();
        assert_eq!(s.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().name, "$1");
    }

    #[test]
    fn shared_source_edits_once_and_selection_preserves_parent() {
        let (mut s, root, child, leaf, a, b) = fixture();
        let parent_selection = s.state.selected_layers.clone();
        let open = s.state.open_comps.clone();
        inline(&mut s, &[a], "layer.select", json!({"layers":[leaf]})).unwrap();
        assert_eq!(s.state.comp_selections[&child].layers, vec![LayerId(leaf)]);
        assert_eq!(s.state.selected_layers, parent_selection);
        let undo = s.history.undo.len();
        inline(&mut s, &[b], "prop.set", json!({"layer":leaf,"path":"transform/opacity","value":42})).unwrap();
        assert_eq!(s.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.prop("transform/opacity").unwrap().value.as_f64(), 42.0);
        assert_eq!(s.history.undo.len(), undo + 1);
        assert_eq!(s.active_comp_id(), Some(root));
        assert_eq!(s.state.open_comps, open);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.prop("transform/opacity").unwrap().value.as_f64(), 100.0);
        assert_eq!(s.active_comp_id(), Some(root));
        s.activate_comp_none();
        s.execute("comp.inlineEdit", json!({"root":root.0,"path":[a],"command":"layer.rename","params":{"layer":leaf,"name":"Headless"}})).unwrap();
        assert_eq!(s.active_comp_id(), None);
    }

    #[test]
    fn offset_reverse_stretch_and_nested_layer_key_time_are_exact() {
        let (mut s, root, child, leaf, a, _) = fixture();
        {
            let p = std::sync::Arc::make_mut(&mut s.project);
            let instance = p.comp_mut(root).unwrap().layer_mut(LayerId(a)).unwrap();
            instance.start_time = Tick::from_seconds_f64(10.0);
            instance.stretch = -200.0;
            let l = p.comp_mut(child).unwrap().layer_mut(LayerId(leaf)).unwrap();
            l.start_time = Tick::from_seconds_f64(1.0);
            l.stretch = 200.0;
        }
        s.set_time(Tick::from_seconds_f64(4.0));
        let c = context(&s, root, &[a], s.time()).unwrap();
        assert_eq!(c.time, Tick::from_seconds_f64(3.0));
        assert_eq!(c.affine, Some((10.0, -2.0)));
        inline(&mut s, &[a], "prop.toggleAnimation", json!({"layer":leaf,"path":"transform/opacity"})).unwrap();
        let p = s.project.comp(child).unwrap().layer(LayerId(leaf)).unwrap().props.prop("transform/opacity").unwrap();
        assert_eq!(p.keys[0].time, Tick::from_seconds_f64(1.0));
        assert_eq!(s.time(), Tick::from_seconds_f64(4.0));
    }

    #[test]
    fn remap_uses_renderer_time_without_claiming_an_inverse() {
        let (mut s, root, _, _, a, _) = fixture();
        s.execute("layer.enableTimeRemap", json!({"layers":[a]})).unwrap();
        s.execute("prop.set", json!({"layer":a,"path":"timeRemap","time":0,"value":7})).unwrap();
        let c = context(&s, root, &[a], Tick::ZERO).unwrap();
        assert_eq!(c.time, Tick::from_seconds_f64(7.0));
        assert!(c.affine.is_none());
    }

    #[test]
    fn stale_cycles_overdepth_and_failed_commands_preserve_state() {
        let (mut s, root, child, leaf, a, _) = fixture();
        let selection = s.state.selected_layers.clone();
        assert!(inline(&mut s, &[a], "prop.set", json!({"layer":leaf,"path":"absent","value":1})).is_err());
        assert_eq!(s.active_comp_id(), Some(root));
        assert_eq!(s.state.selected_layers, selection);
        assert!(context(&s, root, &[u64::MAX], Tick::ZERO).is_err());
        assert!(context(&s, root, &[a; MAX_DEPTH + 1], Tick::ZERO).is_err());
        std::sync::Arc::make_mut(&mut s.project).comp_mut(child).unwrap().layer_mut(LayerId(leaf)).unwrap().source = LayerSource::Comp { item: root };
        assert!(context(&s, root, &[a, leaf], Tick::ZERO).is_err());
        assert!(inline(&mut s, &[a], "file.new", json!({})).is_err());
    }

    #[test]
    fn lock_and_frame_preservation_apply_to_inline_instances() {
        let (mut s, root, child, leaf, a, _) = fixture();
        std::sync::Arc::make_mut(&mut s.project).comp_mut(root).unwrap().layer_mut(LayerId(a)).unwrap().switches.locked = true;
        assert!(inline(&mut s, &[a], "layer.rename", json!({"layer":leaf,"name":"No"})).is_err());
        std::sync::Arc::make_mut(&mut s.project).comp_mut(child).unwrap().preserve_frame_rate = true;
        let c = context(&s, root, &[a], Tick::from_seconds_f64(1.019)).unwrap();
        assert!(c.affine.is_none());
        assert_eq!(c.time, s.project.comp(child).unwrap().frame_rate.tick_of(30));
    }
}
