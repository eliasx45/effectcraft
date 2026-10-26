//! Animation menu: animation presets (EffectCraft's own JSON format), Add Keyframe, Keyframe
//! Assistant (Exponential Scale), text animators/selectors, and the
//! Reveal Properties commands.

use std::collections::BTreeMap;

use effectcraft_keyframe::{Interp, Keyframe, Value as KV};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{GroupKind, Layer, LayerId, LayerSource, Marker, Node, Project, PropGroup, Property, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, bad, frontend, has_comp, has_keys, has_layers, layer_mut, layers_p, match_path_of, selected_leaf_props, str_p};
use crate::{EngineError, KeyRef, Result, Session, cmd};

/// Preset file format version tag (`*.ecpreset`, JSON).
pub const PRESET_TAG: &str = "effectcraftAnimationPreset";
/// The version written: 2 adds `animators` (whole text animators, CX8d). Version 1 files still
/// apply (they have no `animators`). The optional `markers` (the source layer's markers,
/// PRESET-MARKERS) need no new version: files without them apply as before, and a build that
/// doesn't know them applies the rest.
pub const PRESET_VERSION: u32 = 2;

/// A preset marker anchored to the layer's out point (`time` holds out point − marker time);
/// otherwise (`keys`) `time` is the offset from the preset's key origin, like the keys.
const ANCHOR_OUT: &str = "out";
const ANCHOR_KEYS: &str = "keys";

/// The source layer's markers for a preset (layer time, as layer markers are stored): a marker
/// whose start is closer to the layer's out point than to `origin` (the earliest key, or the
/// in point when the preset has no keys) is anchored to the out point, the rest move with the keys.
fn preset_markers(l: &Layer, origin: Tick) -> Vec<Value> {
    let out = l.layer_time(l.out_point);
    l.markers
        .iter()
        .map(|m| {
            let to_out = (out - m.time).abs();
            let to_keys = (m.time - origin).abs();
            let (anchor, offset) = if to_out < to_keys { (ANCHOR_OUT, out - m.time) } else { (ANCHOR_KEYS, m.time - origin) };
            let mut v = serde_json::to_value(Marker { time: offset, ..m.clone() }).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("anchor".into(), json!(anchor));
            }
            v
        })
        .collect()
}

/// A preset's markers: (anchored to the out point, the marker with its stored offset as `time`).
fn markers_of(doc: &Value) -> Vec<(bool, Marker)> {
    doc["markers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let mut m: Marker = serde_json::from_value(v.clone()).ok()?;
                    // Times from a file stay in Tick's documented range (later arithmetic saturates).
                    m.time = m.time.clamp(Tick::MIN, Tick::MAX);
                    m.duration = m.duration.clamp(Tick::ZERO, Tick::MAX);
                    Some((v["anchor"].as_str() == Some(ANCHOR_OUT), m))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Puts a preset's markers on `l`: key-anchored ones at `lt` (the playhead or `at`, layer time) +
/// their offset, out-anchored ones at the layer's out point − theirs, durations as saved. A marker
/// with the same comment is replaced (an unnamed one only at the same time), so applying again
/// doesn't stack them. Returns how many were placed.
fn place_markers(l: &mut Layer, markers: &[(bool, Marker)], lt: Tick) -> usize {
    let out = l.layer_time(l.out_point);
    let placed: Vec<Marker> = markers.iter().map(|(at_out, m)| Marker { time: if *at_out { out - m.time } else { lt + m.time }, ..m.clone() }).collect();
    l.markers.retain(|old| !placed.iter().any(|m| m.comment == old.comment && (!m.comment.is_empty() || m.time == old.time)));
    let n = placed.len();
    l.markers.extend(placed);
    l.markers.sort_by_key(|m| m.time);
    n
}

fn has_selected_props(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    if selected_leaf_props(s).is_empty() && selected_effects(s).is_empty() { Err("select properties or effects first".into()) } else { Ok(()) }
}

fn has_text_layer(s: &Session) -> std::result::Result<(), String> {
    has_layers(s)?;
    let c = s.active_comp().ok_or("no composition")?;
    if s.state.selected_layers.iter().filter_map(|l| c.layer(*l)).any(|l| l.props.group("text/animators").is_some()) {
        Ok(())
    } else {
        Err("select a text layer".into())
    }
}

/// Selected effect groups (layer, effect group).
fn selected_effects(s: &Session) -> Vec<(LayerId, PropGroup)> {
    let Some(c) = s.active_comp() else { return vec![] };
    s.state
        .selected_props
        .iter()
        .filter_map(|(l, u)| {
            let layer = c.layer(*l)?;
            let fx = layer.effects()?;
            fx.groups().find(|g| g.uid == *u).map(|g| (*l, g.clone()))
        })
        .collect()
}

/// Selected text animator groups (layer, `text/animators` child), saved whole (CX8d): each
/// animator with its name, its selectors (range / wiggly / expression, Advanced ▸ Based On,
/// expressions, keys and eases) and its properties. Selecting the layer's Animators group
/// selects all of them.
// A click on an animator (or the Animators) row in the Timeline runs prop.select with that group's
// uid, which puts it in `selected_props`, as Effect Controls does for an effect.
fn selected_animators(s: &Session) -> Vec<(LayerId, PropGroup)> {
    let Some(c) = s.active_comp() else { return vec![] };
    let mut out: Vec<(LayerId, PropGroup)> = vec![];
    for (l, u) in &s.state.selected_props {
        let Some(anims) = c.layer(*l).and_then(|layer| layer.props.group("text/animators")) else { continue };
        let picked: Vec<&PropGroup> = if anims.uid == *u { anims.groups().collect() } else { anims.groups().filter(|g| g.uid == *u).collect() };
        for g in picked {
            if !out.iter().any(|(ol, og)| ol == l && og.uid == g.uid) {
                out.push((*l, g.clone()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------- presets

fn save_preset(s: &mut Session, p: &Value) -> Result<Value> {
    let path = str_p(p, "path").ok_or_else(|| bad("anim.savePreset", "missing `path`"))?.to_string();
    let comp = s.active_comp().ok_or(EngineError::NoComp)?;
    let t = s.time();
    let effects: Vec<(LayerId, PropGroup)> = selected_effects(s);
    let effect_uids: Vec<Uid> = effects.iter().map(|(_, g)| g.uid).collect();
    let mut animators: Vec<(LayerId, PropGroup)> = selected_animators(s);
    let animator_uids: Vec<Uid> = animators.iter().map(|(_, g)| g.uid).collect();
    let mut props = vec![];
    let mut prop_layer: Option<LayerId> = None;
    for (lid, uid) in selected_leaf_props(s) {
        let Some(l) = comp.layer(lid) else { continue };
        // Properties inside a saved effect travel with the effect.
        if l.effects().is_some_and(|fx| fx.groups().any(|g| effect_uids.contains(&g.uid) && g.find(uid).is_some())) {
            continue;
        }
        // So do the properties of a saved text animator.
        if animators.iter().any(|(al, g)| *al == lid && animator_uids.contains(&g.uid) && g.find(uid).is_some()) {
            continue;
        }
        let (Some(pr), Some(mp)) = (l.props.find(uid), match_path_of(&l.props, uid)) else { continue };
        props.push((l.layer_time(t), mp, pr.clone()));
        prop_layer.get_or_insert(lid);
    }
    if props.is_empty() && effects.is_empty() && animators.is_empty() {
        return Err(bad("anim.savePreset", "select properties, effects or text animators to save"));
    }
    // Key times are stored relative to the earliest key (of the properties, the saved effects
    // and the saved text animators alike) so the preset applies at the CTI.
    let mut effects = effects;
    let mut fx_first: Option<Tick> = None;
    for (_, g) in effects.iter().chain(animators.iter()) {
        let mut first_in = |pr: &Property| {
            if let Some(t) = pr.keys.iter().map(|k| k.time).min() {
                fx_first = Some(fx_first.map_or(t, |f| f.min(t)));
            }
        };
        g.walk("", &mut |_, pr| first_in(pr));
    }
    let first_key = props.iter().flat_map(|(_, _, pr)| pr.keys.iter().map(|k| k.time)).chain(fx_first).min();
    let first = first_key.unwrap_or(Tick::ZERO);
    // The layer the selection is on carries its markers along (PRESET-MARKERS): Elias's text
    // presets are timed by "In" / "Out" markers that their expressions read.
    let src_layer = animators.first().map(|(l, _)| *l).or(effects.first().map(|(l, _)| *l)).or(prop_layer).or(s.state.selected_layers.first().copied());
    let markers_json: Vec<Value> =
        src_layer.and_then(|lid| comp.layer(lid)).map(|l| preset_markers(l, first_key.unwrap_or_else(|| l.layer_time(l.in_point)))).unwrap_or_default();
    for (_, g) in effects.iter_mut().chain(animators.iter_mut()) {
        g.walk_mut(&mut |pr| {
            for k in &mut pr.keys {
                k.time -= first;
            }
        });
    }
    let props_json: Vec<Value> = props
        .iter()
        .map(|(_, mp, pr)| {
            let mut pr = pr.clone();
            for k in &mut pr.keys {
                k.time -= first;
            }
            json!({"path": mp, "value": pr.value, "keys": pr.keys, "expression": pr.expr})
        })
        .collect();
    let fx_json: Vec<Value> = effects.iter().map(|(_, g)| serde_json::to_value(g).unwrap_or_default()).collect();
    let name = str_p(p, "name")
        .map(str::to_string)
        .unwrap_or_else(|| std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    let anim_json: Vec<Value> = animators.iter().map(|(_, g)| serde_json::to_value(g).unwrap_or_default()).collect();
    let mut doc = json!({PRESET_TAG: PRESET_VERSION, "name": name, "properties": props_json, "effects": fx_json});
    if !anim_json.is_empty() {
        doc["animators"] = json!(anim_json);
    }
    if !markers_json.is_empty() {
        doc["markers"] = json!(markers_json);
    }
    let text = serde_json::to_string_pretty(&doc).unwrap_or_default();
    // A folder that doesn't exist yet (a new card's assets/<id>/) is made.
    // FsServices::write_file (config::atomic_write) reports a missing folder as NotFound: its
    // temporary file can't be created there.
    let written = match s.services.write_file(&path, text.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => s.services.store_file(&path, text.as_bytes()),
        other => other,
    };
    written.map_err(|e| EngineError::Other(format!("cannot write {path}: {e}")))?;
    Ok(json!({"path": path, "properties": props.len(), "effects": effects.len(), "animators": animators.len(), "markers": markers_json.len()}))
}

fn apply_preset(s: &mut Session, p: &Value) -> Result<Value> {
    let doc: Value = match (p.get("preset"), str_p(p, "path")) {
        (Some(v), _) => v.clone(),
        (None, Some(path)) => {
            let bytes = s.services.read_file(path).map_err(|e| EngineError::Other(format!("cannot read {path}: {e}")))?;
            serde_json::from_slice(&bytes).map_err(|e| bad("anim.applyPreset", format!("not a preset: {e}")))?
        }
        _ => return Err(bad("anim.applyPreset", "missing `path` (or inline `preset`)")),
    };
    if doc.get(PRESET_TAG).is_none() {
        return Err(bad("anim.applyPreset", "not an EffectCraft animation preset"));
    }
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad("anim.applyPreset", "select a layer"));
    }
    // Animation ▸ Recent Animation Presets.
    if let Some(path) = str_p(p, "path") {
        s.prefs.push_recent_preset(path);
        s.save_prefs();
        s.prefs_revision += 1;
    }
    let effects: Vec<PropGroup> =
        doc["effects"].as_array().map(|a| a.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect()).unwrap_or_default();
    // Text animators saved whole (CX8c); text layers only.
    let animators: Vec<PropGroup> =
        doc["animators"].as_array().map(|a| a.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect()).unwrap_or_default();
    // The source layer's markers (PRESET-MARKERS); every layer gets them, text or not.
    let markers = markers_of(&doc);
    // `replace`: same-named effects are replaced too, so applying again doesn't stack them
    // (text animators always are).
    let replace = super::b_p(p, "replace").unwrap_or(false);
    let props: Vec<(String, Property)> = doc["properties"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let path = v["path"].as_str()?.to_string();
                    let value: KV = serde_json::from_value(v["value"].clone()).ok()?;
                    let keys: Vec<Keyframe> = serde_json::from_value(v["keys"].clone()).unwrap_or_default();
                    let expr = serde_json::from_value(v["expression"].clone()).ok().flatten();
                    let mut pr = Property::new(0, "", "", value);
                    pr.keys = keys;
                    pr.expr = expr;
                    Some((path, pr))
                })
                .collect()
        })
        .unwrap_or_default();
    // `at` (comp seconds): where the preset's keys start; default the CTI.
    let t = match super::f_p(p, "at").filter(|x| x.is_finite() && x.abs() <= 86_400.0) {
        Some(x) => Tick::from_seconds_f64(x),
        None => s.time(),
    };
    let mut applied = 0;
    let mut placed_markers = 0;
    let mut skipped = vec![];
    s.edit("Apply Animation Preset", None, |proj, _| {
        let mut next = proj.next_id;
        for lid in &ids {
            let l = layer_mut(proj, cid, *lid)?;
            let lt = l.layer_time(t);
            if !markers.is_empty() {
                if l.markers_locked {
                    skipped.push("markers (the layer's markers are locked)".to_string());
                } else {
                    placed_markers += place_markers(l, &markers, lt);
                }
            }
            if !animators.is_empty() {
                match l.props.group_mut("text/animators") {
                    Some(anims) => {
                        for g in &animators {
                            // Same name = the same animator: applying again replaces it (the
                            // names are kept exactly: expressions find them by name).
                            anims.children.retain(|c| !matches!(c, Node::Group(x) if x.match_id == g.match_id && x.name == g.name));
                            let mut g = g.clone();
                            g.reassign_uids(&mut next);
                            // reassign_uids leaves `next` on the last uid it gave.
                            next += 1;
                            g.walk_mut(&mut |pr| {
                                for k in &mut pr.keys {
                                    k.time += lt;
                                }
                            });
                            anims.children.push(Node::Group(g));
                            applied += 1;
                        }
                    }
                    None => skipped.push("text animators (not a text layer)".to_string()),
                }
            }
            if let Some(fx) = l.props.sub_mut("effects") {
                for g in &effects {
                    if replace {
                        fx.children.retain(|c| !matches!(c, Node::Group(x) if x.match_id == g.match_id && x.name == g.name));
                    }
                    let mut g = g.clone();
                    g.reassign_uids(&mut next);
                    g.walk_mut(&mut |pr| {
                        for k in &mut pr.keys {
                            k.time += lt;
                        }
                    });
                    next += 1;
                    fx.children.push(Node::Group(g));
                    applied += 1;
                }
            }
            for (path, src) in &props {
                match l.props.prop_mut(path) {
                    Some(pr) if std::mem::discriminant(&pr.value) == std::mem::discriminant(&src.value) => {
                        pr.value = src.value.clone();
                        pr.keys = src
                            .keys
                            .iter()
                            .cloned()
                            .map(|mut k| {
                                k.time += lt;
                                k
                            })
                            .collect();
                        if src.expr.is_some() {
                            pr.expr = src.expr.clone();
                        }
                        applied += 1;
                    }
                    _ => skipped.push(path.clone()),
                }
            }
        }
        proj.next_id = next;
        Ok(())
    })?;
    Ok(json!({"applied": applied, "markers": placed_markers, "skipped": skipped}))
}

// ---------------------------------------------------------------- keyframes

fn add_keyframe(s: &mut Session, _: &Value) -> Result<Value> {
    let targets = selected_leaf_props(s);
    if targets.is_empty() {
        return Err(bad("anim.addKeyframe", "select properties first"));
    }
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    let t = s.time();
    let n = s.edit("Add Keyframe", None, |proj, _| {
        let mut n = 0;
        for (lid, uid) in &targets {
            let l = layer_mut(proj, cid, *lid)?;
            let lt = l.layer_time(t);
            let Some(pr) = l.props.find_mut(*uid) else { continue };
            if pr.static_only {
                continue;
            }
            if pr.keys.is_empty() {
                pr.set_animated(true, lt);
            } else {
                let v = pr.value_at(lt);
                pr.set_value_at(lt, v);
            }
            n += 1;
        }
        Ok(n)
    })?;
    Ok(json!({"keys": n}))
}

/// Selected keys grouped by (layer, property).
fn keys_by_prop(s: &Session) -> BTreeMap<(LayerId, Uid), Vec<Tick>> {
    let mut m: BTreeMap<(LayerId, Uid), Vec<Tick>> = BTreeMap::new();
    for k in &s.state.selected_keys {
        m.entry((k.layer, k.prop)).or_default().push(k.time);
    }
    m
}

fn exponential_scale(s: &mut Session, _: &Value) -> Result<Value> {
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fd = comp.frame_duration();
    let groups = keys_by_prop(s);
    let mut done = 0;
    let mut work = vec![];
    for ((lid, uid), mut times) in groups {
        let Some(l) = comp.layer(lid) else { continue };
        if l.transform().and_then(|tr| tr.get("scale")).map(|p| p.uid) != Some(uid) {
            continue;
        }
        times.sort();
        if times.len() >= 2 {
            work.push((lid, uid, times[0], *times.last().unwrap_or(&times[0])));
        }
    }
    if work.is_empty() {
        return Err(bad("keys.exponentialScale", "select two Scale keyframes"));
    }
    s.edit("Exponential Scale", None, |proj, st| {
        for (lid, uid, a, b) in &work {
            let l = layer_mut(proj, cid, *lid)?;
            let Some(pr) = l.props.find_mut(*uid) else { continue };
            let (va, vb) = (pr.value_at(*a).as_vec3(), pr.value_at(*b).as_vec3());
            pr.keys.retain(|k| k.time <= *a || k.time >= *b);
            let n = ((*b - *a).0 / fd.0.max(1)).max(1);
            for i in 0..=n {
                let u = i as f64 / n as f64;
                let t = if i == n { *b } else { *a + Tick(fd.0 * i) };
                let f = |x: f64, y: f64| if x.abs() < 1e-9 || y.abs() < 1e-9 || x.signum() != y.signum() { x + (y - x) * u } else { x * (y / x).powf(u) };
                let v = [f(va[0], vb[0]), f(va[1], vb[1]), f(va[2], vb[2])];
                let mut k = Keyframe::new(t, KV::Vec3(v));
                k.in_interp = Interp::Linear;
                k.out_interp = Interp::Linear;
                effectcraft_keyframe::set_key(&mut pr.keys, k);
                st.selected_keys.push(KeyRef { layer: *lid, prop: *uid, time: t });
            }
            done += 1;
        }
        st.selected_keys.sort_by_key(|k| (k.layer.0, k.prop, k.time));
        st.selected_keys.dedup();
        Ok(())
    })?;
    Ok(json!({"properties": done}))
}

// ---------------------------------------------------------------- text animators

/// Animation ▸ Add Text Selector ▸ Range / Wiggly / Expression (see `layer.addTextSelector`).
fn add_selector(s: &mut Session, p: &Value) -> Result<Value> {
    super::text_anim::add_selector(s, p)
}

fn remove_all_animators(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, ids) = layers_p(s, p)?;
    s.edit("Remove All Text Animators", None, |proj, _| {
        for lid in &ids {
            if let Some(a) = layer_mut(proj, cid, *lid)?.props.group_mut("text/animators") {
                a.children.clear();
            }
        }
        Ok(())
    })?;
    Ok(Value::Null)
}

// ---------------------------------------------------------------- reveal

/// Whether a static property differs from the value a fresh layer would have. Transform
/// defaults depend on the source size and comp size.
fn modified(pr: &Property, group: &str, default_anchor: [f64; 2], comp_center: [f64; 2]) -> bool {
    if pr.is_animated() || pr.expr.is_some() {
        return true;
    }
    let v = &pr.value;
    let near = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
    match (group, pr.match_id.as_str()) {
        ("transform", "anchor") => !near(&v.as_vec2(), &default_anchor),
        ("transform", "position") => !near(&v.as_vec2(), &comp_center) || v.as_vec3()[2].abs() > 1e-6,
        // Separate Dimensions: X and Y Position default to the comp centre.
        ("transform", "positionX") => (v.as_f64() - comp_center[0]).abs() > 1e-6,
        ("transform", "positionY") => (v.as_f64() - comp_center[1]).abs() > 1e-6,
        ("transform", "scale") => !near(&v.as_vec3(), &[100.0; 3]),
        ("transform", "opacity") => (v.as_f64() - 100.0).abs() > 1e-6,
        ("transform", "orientation") => !near(&v.as_vec3(), &[0.0; 3]),
        ("transform", _) => v.as_f64().abs() > 1e-6,
        _ => false,
    }
}

/// The group a fresh layer of `l`'s kind (`fresh`) has in place of `top`, for the groups whose
/// defaults are fixed: Audio, Material / Geometry / Camera / Light Options, a text layer's Path
/// and More Options, and a camera's or light's Transform (whose defaults aren't a 2D layer's).
fn default_group(top: &PropGroup, l: &Layer, fresh: &Layer) -> Option<PropGroup> {
    let mut next = 0;
    let mut ids = Ids(&mut next);
    let camera_or_light = matches!(l.source, LayerSource::Camera | LayerSource::Light { .. });
    let mut g = match top.match_id.as_str() {
        "transform" if !camera_or_light => return None,
        // Added to a layer after it is made (a source with audio, an Advanced 3D comp).
        "audio" => build::audio(&mut ids),
        "geometryOptions" if matches!(l.source, LayerSource::Text | LayerSource::Shape) => build::extrusion_geometry_options(&mut ids),
        "transform" | "cameraOptions" | "lightOptions" | "materialOptions" | "geometryOptions" | "text" => fresh.props.sub(&top.match_id)?.clone(),
        _ => return None,
    };
    match top.match_id.as_str() {
        // Source Text and the animators are the layer's content, not options with a default.
        "text" => g.children.retain(|c| matches!(c.match_id(), "pathOptions" | "moreOptions")),
        // A model's scale and clip come from its file.
        "geometryOptions" if matches!(l.source, LayerSource::Model { .. }) => g.children.retain(|c| !matches!(c.match_id(), "unitScale" | "animation")),
        _ => {}
    }
    Some(g)
}

/// Static properties of `g` whose value differs from the same property of `default` (matched by
/// match id; properties `default` lacks have no known default and are left out).
fn changed_from(g: &PropGroup, default: &PropGroup, out: &mut Vec<Uid>) {
    for c in &g.children {
        match c {
            Node::Prop(pr) => {
                if default.get(&pr.match_id).is_some_and(|d| d.value != pr.value) && !out.contains(&pr.uid) {
                    out.push(pr.uid);
                }
            }
            Node::Group(sg) => {
                if let Some(d) = default.sub(&sg.match_id) {
                    changed_from(sg, d, out);
                }
            }
        }
    }
}

/// What of effect group `g` differs from `new`, a new instance of the effect: parameters with
/// another value, and in full the groups a new instance lacks (puppet meshes and pins, paint
/// strokes: what the user made).
fn effect_changes(g: &PropGroup, new: &PropGroup, out: &mut Vec<Uid>) {
    for c in &g.children {
        match c {
            Node::Prop(pr) => {
                if new.get(&pr.match_id).is_some_and(|d| d.value != pr.value) && !out.contains(&pr.uid) {
                    out.push(pr.uid);
                }
            }
            Node::Group(sg) => match new.sub(&sg.match_id) {
                Some(d) => effect_changes(sg, d, out),
                None => out.push(sg.uid),
            },
        }
    }
}

fn reveal(s: &mut Session, p: &Value) -> Result<Value> {
    let kind = str_p(p, "kind").unwrap_or("keyframes").to_string();
    let cid = super::comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let layers: Vec<LayerId> = if s.state.selected_layers.is_empty() { comp.layers.iter().map(|l| l.id).collect() } else { s.state.selected_layers.clone() };
    let mut out = vec![];
    for lid in &layers {
        let Some(l) = comp.layer(*lid) else { continue };
        let (w, h) = effectcraft_render::source_size(&s.project, l);
        let anchor = [w as f64 / 2.0, h as f64 / 2.0];
        let center = [comp.width as f64 / 2.0, comp.height as f64 / 2.0];
        // Properties at the top of the layer's tree (Time Remap): enabled time remapping is a
        // modification in itself, so it reveals even with no keys left on it (#990).
        for pr in l.props.props() {
            let hit = match kind.as_str() {
                "keyframes" => pr.is_animated() || pr.match_id == "timeRemap",
                "animation" => pr.is_animated() || pr.has_expression() || pr.match_id == "timeRemap",
                _ => pr.match_id == "timeRemap" || modified(pr, "", anchor, center),
            };
            if hit {
                out.push(json!({"layer": lid.0, "prop": pr.uid}));
            }
        }
        // Modified: a fresh layer of the same kind gives the defaults to compare against.
        let fresh = (kind == "modified").then(|| build::layer(&mut Project::default(), comp, "", l.source.clone(), (w, h), None));
        let camera_or_light = matches!(l.source, LayerSource::Camera | LayerSource::Light { .. });
        for top in l.props.groups() {
            let hit = |pr: &Property| match kind.as_str() {
                "keyframes" => pr.is_animated(),
                "animation" => pr.is_animated() || pr.has_expression(),
                // A camera's or light's Transform is compared with a fresh one's below.
                _ if camera_or_light && top.match_id == "transform" => pr.is_animated() || pr.expr.is_some(),
                _ => modified(pr, &top.match_id, anchor, center),
            };
            let mut found = vec![];
            top.walk("", &mut |_, pr| {
                if hit(pr) {
                    found.push(pr.uid);
                }
            });
            // Modified: masks count as modifications in full (AE reveals them).
            if kind == "modified" && top.match_id == "masks" {
                for g in top.groups() {
                    found.push(g.uid);
                }
            }
            // Modified: an effect's parameters that differ from a new instance's.
            if kind == "modified" && top.match_id == "effects" {
                let size = effectcraft_render::effect_bounds(&s.project, comp, l).0;
                for g in top.groups() {
                    let spec = match &g.kind {
                        GroupKind::Effect { effect } => effectcraft_effects::find(effect),
                        _ => None,
                    };
                    // A missing plug-in's defaults aren't known: the effect shows in full.
                    let Some(spec) = spec else {
                        found.push(g.uid);
                        continue;
                    };
                    let mut next = 0;
                    let new = effectcraft_effects::instantiate(spec, &mut Ids(&mut next), "", size);
                    effect_changes(g, &new, &mut found);
                }
            }
            if kind == "modified" && top.match_id == "text" {
                for g in top.group("animators").into_iter().flat_map(|a| a.groups()) {
                    found.push(g.uid);
                }
            }
            if let Some(fresh) = &fresh {
                if let Some(default) = default_group(top, l, fresh) {
                    changed_from(top, &default, &mut found);
                }
                // Every layer style counts in full, like an effect; so does each overridden
                // Essential Property.
                if top.match_id == effectcraft_project::styles::GROUP {
                    found.extend(top.groups().filter(|g| g.match_id != effectcraft_project::styles::BLENDING).map(|g| g.uid));
                }
                if let GroupKind::Essential { overridden } = &top.kind {
                    for u in overridden {
                        if !found.contains(u) && (top.find(*u).is_some() || top.find_group(*u).is_some()) {
                            found.push(*u);
                        }
                    }
                }
            }
            out.extend(found.into_iter().map(|u| json!({"layer": lid.0, "prop": u})));
        }
    }
    let layers: Vec<u64> = layers.iter().map(|l| l.0).collect();
    frontend(s, "timeline.revealProps", &json!({"props": out, "kind": kind, "layers": layers}))?;
    Ok(json!({"props": out}))
}

/// Is `g` an effect/mask group kind (used by tests).
#[allow(dead_code)]
pub(crate) fn is_indexed(g: &PropGroup) -> bool {
    matches!(g.kind, GroupKind::Indexed | GroupKind::Mask { .. } | GroupKind::Effect { .. })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "anim.savePreset",
            "Save Animation Preset...",
            ["Animation"],
            None,
            "{path (.ecpreset), name?} — saves the selected properties, effects and text animators (whole), and their layer's markers (near the out point: kept that far from it; else: with the keys)",
            has_selected_props,
            save_preset
        ),
        cmd!(
            "anim.applyPreset",
            "Apply Animation Preset...",
            ["Animation"],
            None,
            "{path | preset, layers?, at? (comp s: where the keys start; default the CTI), replace? (bool: same-named effects are replaced, not stacked; text animators of the same name always are)} — the preset's markers go on every layer (same-comment markers replaced)",
            has_layers,
            apply_preset
        ),
        cmd!("anim.addKeyframe", "Add Keyframe", ["Animation"], None, "{} — keys the selected properties at the CTI", has_selected_props, add_keyframe),
        cmd!(
            "keys.exponentialScale",
            "Exponential Scale",
            ["Animation", "Keyframe Assistant"],
            None,
            "{} — two selected Scale keys",
            has_keys,
            exponential_scale
        ),
        cmd!("text.addSelector", "Add Text Selector", [], None, "{layer?, animator?: uid|\"#n\", kind: range|wiggly|expression}", has_text_layer, add_selector),
        cmd!("text.removeAllAnimators", "Remove All Text Animators", ["Animation"], None, "{layers?}", has_text_layer, remove_all_animators),
        cmd!("anim.reveal", "Reveal Properties", [], None, "{kind: keyframes|animation|modified}", has_comp, reveal),
    ]
}
