//! The command registry. Ids follow After Effects' menu structure; every menu item, panel button,
//! shortcut and viewer/timeline gesture maps to one of these.
//!
//! Every file of this folder is a command module, declared by build.rs; its
//! `pub fn specs() -> Vec<CommandSpec>` is registered in [`command_specs`]. Adding commands is
//! adding a file (docs/architecture.md, "Self-registering modules"). The modules below need `pub`.

include!(concat!(env!("OUT_DIR"), "/commands.rs"));
pub mod anim_tools;
// Batch 1 lane C: the UI reads card masks (CX1).
pub mod cards;
pub mod content_fill;
pub mod essential;
pub mod expr_tools;
pub mod footage_panel;
pub mod graph_cmds;
pub mod inline_comp;
pub mod markers;
pub mod mask_interp;
pub mod paint;
pub mod panels_cmds;
pub mod puppet;
pub mod relink_moved;
pub mod render_compare;
pub mod roto_cmds;
pub mod round_corners;
pub mod scripts;
pub mod shape_stroke;
pub mod shape_tool;
pub mod text_edit;
pub mod time;
pub mod viewer_cmds;
#[cfg(test)]
pub(crate) use app_more::report_text as report_text_for_tests;
pub(crate) use app_more::view_command;
pub(crate) use comp_more::encode_png;
#[cfg(test)]
pub(crate) use file::open as open_for_tests;
#[cfg(test)]
pub(crate) use keys_more::amplitudes as amplitudes_for_tests;
#[cfg(test)]
pub(crate) use mask::split_segment as split_segment_for_tests;

use std::sync::OnceLock;

use effectcraft_project::{Comp, ItemId, Layer, LayerId, PropGroup};
use effectcraft_time::Tick;
use serde_json::Value;

use crate::{EngineError, Result, Session};

pub(crate) use target_refs::{checked_layer_ref, validate_target_refs};

pub type Run = fn(&mut Session, &Value) -> Result<Value>;
pub type Enabled = fn(&Session) -> std::result::Result<(), String>;

/// Metadata + implementation for one command.
pub struct CommandSpec {
    pub id: &'static str,
    pub label: &'static str,
    /// Menu placement, e.g. `["Layer", "New"]`. Empty = not in menus.
    pub menu: &'static [&'static str],
    /// Default shortcut (egui-style names, `Cmd` = ⌘ on macOS / Ctrl elsewhere).
    pub shortcut: Option<&'static str>,
    /// Further default shortcuts (the menus and tooltips show `shortcut`).
    pub alt_shortcuts: &'static [&'static str],
    /// Parameter description for agents and the MCP schema.
    pub params: &'static str,
    pub enabled: Enabled,
    pub run: Run,
    /// Record in the journal (false for pure queries).
    pub journal: bool,
}

#[macro_export]
macro_rules! cmd {
    // Several default shortcuts: `["PageDown", "Cmd+ArrowRight"]`.
    ($id:literal, $label:literal, [$($m:literal),*], [$sc:literal, $($alt:literal),+], $params:literal, $en:expr, $run:expr) => {
        $crate::commands::CommandSpec { alt_shortcuts: &[$($alt),+], ..$crate::cmd!($id, $label, [$($m),*], Some($sc), $params, $en, $run) }
    };
    ($id:literal, $label:literal, [$($m:literal),*], $sc:expr, $params:literal, $en:expr, $run:expr) => {
        $crate::commands::CommandSpec {
            id: $id,
            label: $label,
            menu: &[$($m),*],
            shortcut: $sc,
            alt_shortcuts: &[],
            params: $params,
            enabled: $en,
            run: $run,
            journal: true,
        }
    };
}
#[macro_export]
macro_rules! query {
    ($id:literal, $label:literal, $params:literal, $run:expr) => {
        $crate::commands::CommandSpec {
            id: $id,
            label: $label,
            menu: &[],
            shortcut: None,
            alt_shortcuts: &[],
            params: $params,
            enabled: $crate::commands::always,
            run: $run,
            journal: false,
        }
    };
}

/// Registration order of the command modules that existed when registration became automatic
/// (AG3). The command list's order is observable (`list_commands`, scripting's command indices,
/// the first command to claim a shortcut), so it stays as it was. Modules added since come after
/// these, by file name: don't add to this list. `crate::x` names a module next to `lib.rs`.
pub(crate) const LEGACY_ORDER: &[&str] = &[
    "file",
    "crate::models",
    "edit",
    "comp",
    "layer",
    "styles",
    "three_d",
    "model3d",
    "rig3d",
    "text_anim",
    "text_edit",
    "layer_time",
    "prop",
    "prop_groups",
    "project_items",
    "anim",
    "anim_tools",
    "link",
    "markers",
    "mask",
    "paths",
    "effect",
    "time",
    "render_queue",
    "help",
    "journal",
    "query",
    "batch",
    "crate::figma",
    "layer_menu",
    "create",
    "animation",
    "view",
    "viewer_cmds",
    "shape_stroke",
    "shape_tool",
    "focus",
    "key_labels",
    "key_transform",
    "file_more",
    "lottie",
    "aep",
    "timeline",
    "comp_more",
    "frame_export",
    "watch_folder",
    "vpe",
    "path_nulls",
    "vr_editor",
    "frontend",
    "track",
    "mask_interp",
    "warp_cmds",
    "camera_cmds",
    "roto_cmds",
    "paint",
    "puppet",
    "liquify",
    "settings",
    "app_more",
    "keys_more",
    "essential",
    "expr_tools",
    "proxy",
    "scripts",
    "panels_cmds",
    "footage_panel",
    "autotrace",
    "scene_detect",
    "align_data",
    "align",
    "content_fill",
    "stubs",
    "crate::storage",
    "crate::learn",
    "crate::templates",
    "crate::preview",
    "render_compare",
];

/// Every command module's `specs()`, in registration order: [`LEGACY_ORDER`], then the others
/// (`commands/*.rs`, then `src/*.rs` as `crate::x`) by file name.
pub(crate) fn command_modules() -> Vec<(String, fn() -> Vec<CommandSpec>)> {
    let all: Vec<(String, fn() -> Vec<CommandSpec>)> =
        COMMAND_MODULES.iter().map(|(n, f)| (n.to_string(), *f)).chain(crate::COMMAND_MODULES.iter().map(|(n, f)| (format!("crate::{n}"), *f))).collect();
    let mut out: Vec<(String, fn() -> Vec<CommandSpec>)> = LEGACY_ORDER.iter().filter_map(|name| all.iter().find(|(n, _)| n == name).cloned()).collect();
    out.extend(all.into_iter().filter(|(n, _)| !LEGACY_ORDER.contains(&n.as_str())));
    out
}

pub fn command_specs() -> &'static [CommandSpec] {
    static SPECS: OnceLock<Vec<CommandSpec>> = OnceLock::new();
    SPECS.get_or_init(|| command_modules().into_iter().flat_map(|(_, specs)| specs()).collect())
}

/// Set `footage` as the proxy of `item` (File ▸ Create Proxy's post-render action).
pub(crate) fn proxy_set(s: &mut Session, item: ItemId, footage: effectcraft_project::Footage) -> Result<()> {
    proxy::set_proxy_footage(s, &[item], footage)
}

/// (id, name) of the text animation presets.
pub fn text_preset_list() -> Vec<(String, String)> {
    text_anim::preset_names()
}

/// (id, name, category) of the built-in animation presets (Transitions…).
pub fn builtin_preset_list() -> Vec<(String, String, String)> {
    anim_presets::preset_names()
}

pub fn find(id: &str) -> Option<&'static CommandSpec> {
    // Every command execution looks its spec up: index the ~2,000 ids once.
    static INDEX: OnceLock<std::collections::HashMap<&'static str, usize>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let mut m = std::collections::HashMap::new();
        for (i, c) in command_specs().iter().enumerate() {
            m.entry(c.id).or_insert(i);
        }
        m
    });
    index.get(id).map(|&i| &command_specs()[i])
}

// ---------- parameter validation (agents) ----------

/// Top-level keys accepted by a params doc such as `{layers: [id|name|#n], add?, toggle?}` or
/// `{time? (s) | frame? | timecode?}`; `None` when the doc isn't a `{…}` key list.
pub fn accepted_params(doc: &str) -> Option<Vec<String>> {
    let body = doc.trim().strip_prefix('{')?;
    // Top-level pieces up to the matching close brace, split at `,` and `|`; `:` starts a value.
    let (mut keys, mut cur, mut depth, mut in_value) = (Vec::new(), String::new(), 0i32, false);
    let push = |cur: &mut String, keys: &mut Vec<String>| {
        let k: String = cur.trim().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !k.is_empty() && !keys.contains(&k) {
            keys.push(k);
        }
        cur.clear();
    };
    for c in body.chars() {
        match c {
            '{' | '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            '}' if depth == 0 => {
                push(&mut cur, &mut keys);
                return Some(keys);
            }
            '}' => depth -= 1,
            ',' if depth == 0 => {
                push(&mut cur, &mut keys);
                in_value = false;
            }
            '|' if depth == 0 && !in_value => push(&mut cur, &mut keys),
            ':' if depth == 0 => {
                push(&mut cur, &mut keys);
                in_value = true;
            }
            _ if depth == 0 && !in_value => cur.push(c),
            _ => {}
        }
    }
    None
}

/// Top-level pieces of a params doc (split at depth-0 commas), e.g. `{layer?, path|prop, value}`
/// → `["layer?", "path|prop", "value"]`.
fn doc_pieces(doc: &str) -> Option<Vec<String>> {
    let body = doc.trim().strip_prefix('{')?;
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for c in body.chars() {
        match c {
            '{' | '[' | '(' => depth += 1,
            ']' | ')' => depth -= 1,
            '}' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                return Some(out);
            }
            '}' => depth -= 1,
            ',' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    None
}

/// The JSON Schema of a command's parameters, derived from its params doc: one property per
/// accepted key (described by its piece of the doc), no others. MCP `describe_command` and
/// `list_commands {schemas: true}` serve it.
pub fn params_schema(spec: &CommandSpec) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = vec![];
    for piece in doc_pieces(spec.params).unwrap_or_default() {
        let keys = accepted_params(&format!("{{{piece}}}")).unwrap_or_default();
        let alternatives = keys.len() > 1;
        for k in keys {
            // `key?` is optional; so is any key with alternatives (`path|prop`).
            let optional = alternatives || piece.contains(&format!("{k}?"));
            if !optional && !required.contains(&k) {
                required.push(k.clone());
            }
            let mut sch = serde_json::json!({"description": piece});
            if let Some(t) = param_type(&k, &piece) {
                sch["type"] = t;
            }
            props.insert(k, sch);
        }
    }
    serde_json::json!({
        "type": "object",
        "description": spec.params,
        "properties": props,
        "required": required,
        "additionalProperties": accepted_params(spec.params).is_none(),
    })
}

/// The JSON Schema `type` of parameter `key`, read from its piece of the params doc (`index
/// (1-based)`, `add?: bool`, `layers: [id|name|#n]`, `time? (s)`); `None` when the doc doesn't say.
/// Index-like keys are integers; their docs say whether they count from 0 or 1.
fn param_type(key: &str, piece: &str) -> Option<Value> {
    // The doc after this key (`time? (s) | frame? | timecode?` says nothing about `timecode`).
    let rest = piece.strip_prefix(key).map(|r| r.trim_start_matches('?').trim_start()).unwrap_or("");
    let value = rest.strip_prefix(':').map(str::trim).unwrap_or("");
    let t = if key == "index" || key.ends_with("Index") {
        serde_json::json!("integer")
    } else if value.starts_with('[') {
        serde_json::json!("array")
    } else if value.starts_with('{') {
        serde_json::json!("object")
    } else if value.starts_with("bool") || rest.starts_with("(bool") {
        serde_json::json!("boolean")
    } else if ["layer", "comp", "item", "effect", "mask"].contains(&key) {
        serde_json::json!(["integer", "string"])
    } else if ["layers", "items", "keys", "effects"].contains(&key) {
        serde_json::json!("array")
    } else if key == "time" || rest.starts_with("(s)") || rest.starts_with("(comp s)") || rest.starts_with("(s,") {
        serde_json::json!("number")
    } else {
        return None;
    };
    Some(t)
}

/// Keys every command understands (comp targeting, gesture merging) and accepted aliases.
// `expectRevision` and `client` are execute parameters every command takes (see `Session::execute`).
const ALWAYS_OK: &[&str] = &["comp", "merge", "expectRevision", "client"];
const ALIASES: &[(&str, &str)] = &[("layers", "layer"), ("layer", "layers"), ("time", "frame"), ("frameRate", "fps"), ("properties", "property")];

/// Reject unknown top-level parameters (typos, guessed names) with the list of accepted keys.
pub fn check_params(spec: &CommandSpec, params: &Value) -> Result<()> {
    let (Some(obj), Some(accepted)) = (params.as_object(), accepted_params(spec.params)) else { return Ok(()) };
    let ok = |k: &str| ALWAYS_OK.contains(&k) || accepted.iter().any(|a| a == k || ALIASES.iter().any(|(doc, alias)| doc == a && *alias == k));
    let unknown: Vec<&str> = obj.keys().map(String::as_str).filter(|k| !ok(k)).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let list = if accepted.is_empty() { "none".to_string() } else { accepted.join(", ") };
    Err(bad(
        spec.id,
        format!("unknown parameter(s) {}; accepted: {list} (doc: {})", unknown.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", "), spec.params),
    ))
}

// ---------- enablement ----------

pub fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}
pub(crate) fn has_comp(s: &Session) -> std::result::Result<(), String> {
    s.active_comp().map(|_| ()).ok_or_else(|| "no composition is open".into())
}
pub(crate) fn has_layers(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    if s.state.selected_layers.is_empty() { Err("select a layer first".into()) } else { Ok(()) }
}
pub(crate) fn has_keys(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    if s.state.selected_keys.is_empty() { Err("select keyframes first".into()) } else { Ok(()) }
}

pub(crate) fn has_project_selection(s: &Session) -> std::result::Result<(), String> {
    if s.state.project_selection.is_empty() { Err("select an item in the Project panel first".into()) } else { Ok(()) }
}

/// Disabled-command enablement (menu entries whose implementation lands with another milestone;
/// none are left, kept for future stubs).
#[allow(dead_code)]
pub(crate) fn not_yet(_: &Session) -> std::result::Result<(), String> {
    Err("not available yet in EffectCraft".into())
}

/// `run` of a not-yet-available command.
#[allow(dead_code)]
pub(crate) fn not_yet_run(_: &mut Session, _: &Value) -> Result<Value> {
    Err(EngineError::Other("not available yet in EffectCraft".into()))
}

/// Hand a frontend-only command to the UI ([`crate::Event::Frontend`]).
pub(crate) fn frontend(s: &mut Session, id: &str, p: &Value) -> Result<Value> {
    s.events.push(crate::Event::Frontend { command: id.to_string(), params: p.clone() });
    Ok(serde_json::json!({"frontend": id}))
}

/// Match path of a node (`transform/position`, `effects/#2/blurriness` style with `match#n`
/// occurrences) so it can be found again on another layer.
pub fn match_path_of(g: &effectcraft_project::PropGroup, uid: effectcraft_project::Uid) -> Option<String> {
    for c in &g.children {
        let nth = g.children.iter().take_while(|x| !std::ptr::eq(*x, c)).filter(|x| x.match_id() == c.match_id()).count() + 1;
        let seg = if nth == 1 { c.match_id().to_string() } else { format!("{}#{nth}", c.match_id()) };
        if c.uid() == uid {
            return Some(seg);
        }
        if let effectcraft_project::Node::Group(sub) = c
            && let Some(rest) = match_path_of(sub, uid)
        {
            return Some(format!("{seg}/{rest}"));
        }
    }
    None
}

/// Selected properties (leaves) of the active comp as (layer, uid); selected groups expand to
/// their properties.
pub(crate) fn selected_leaf_props(s: &Session) -> Vec<(LayerId, effectcraft_project::Uid)> {
    let Some(comp) = s.active_comp() else { return vec![] };
    let mut out = vec![];
    for (lid, uid) in &s.state.selected_props {
        let Some(l) = comp.layer(*lid) else { continue };
        if l.props.find(*uid).is_some() {
            out.push((*lid, *uid));
        } else if let Some(g) = l.props.find_group(*uid) {
            g.walk("", &mut |_, p| out.push((*lid, p.uid)));
        }
    }
    out
}

// ---------- params ----------

pub(crate) fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}
pub(crate) fn str_p<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}
pub(crate) fn f_p(p: &Value, k: &str) -> Option<f64> {
    p.get(k).and_then(Value::as_f64)
}
pub(crate) fn b_p(p: &Value, k: &str) -> Option<bool> {
    p.get(k).and_then(Value::as_bool)
}
pub(crate) fn merge_p(p: &Value) -> Option<&str> {
    str_p(p, "merge")
}

/// `comp`: item id or name (an all-digit string that names no comp is taken as an id, as MCP
/// clients often send numbers as strings); default the active comp.
pub(crate) fn comp_id(s: &Session, p: &Value) -> Result<ItemId> {
    let missing = |v: &Value| EngineError::NoSuchComp(v.to_string());
    match p.get("comp") {
        Some(v @ Value::Number(n)) => n.as_u64().map(ItemId).filter(|id| s.project.comp(*id).is_some()).ok_or_else(|| missing(v)),
        Some(v @ Value::String(name)) => {
            let candidates: Vec<_> = s.project.items.values().filter(|i| &i.name == name && i.as_comp().is_some()).collect();
            if candidates.len() > 1 {
                let candidates: Vec<_> =
                    candidates.iter().map(|i| serde_json::json!({"id": i.id.0, "name": i.name, "parent": i.parent.map(|p| p.0)})).collect();
                return Err(bad(
                    "comp",
                    format!("ambiguous composition name {name:?}; candidates: {}; use a numeric composition id", Value::Array(candidates)),
                ));
            }
            candidates.first().map(|i| i.id).or_else(|| digits_id(name).map(ItemId).filter(|id| s.project.comp(*id).is_some())).ok_or_else(|| missing(v))
        }
        _ => s.active_comp_id().ok_or(EngineError::NoComp),
    }
}

/// An all-ASCII-digit string as a number (`"98"` -> 98); anything else (names, `#n`, signs) is `None`.
fn digits_id(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Resolve a layer reference: id (a number, never a stack position: an unknown id is `None`
/// rather than silently another layer), `"#n"` (1-based stack position) or name. An all-digit
/// string that names no layer is resolved as the id it spells.
pub(crate) fn resolve_layer(comp: &Comp, v: &Value) -> Option<LayerId> {
    match v {
        Value::Number(n) => {
            let id = LayerId(n.as_u64()?);
            comp.layer(id).map(|_| id)
        }
        Value::String(s) => {
            if let Some(i) = s.strip_prefix('#').and_then(|i| i.parse::<usize>().ok()) {
                return comp.layers.get(i.checked_sub(1)?).map(|l| l.id);
            }
            let mut named = comp.layers.iter().filter(|l| &l.name == s);
            let first = named.next();
            if named.next().is_some() {
                return None;
            }
            first.map(|l| l.id).or_else(|| digits_id(s).and_then(|id| resolve_layer(comp, &Value::from(id))))
        }
        _ => None,
    }
}

/// The error text for a layer reference that matches no layer.
pub(crate) fn no_layer(v: &Value) -> String {
    format!("no layer {v} (a number is a layer id; use \"#n\" for the n-th layer from the top, or a name from comp.info)")
}

/// Pick a group (an effect, a mask…) by reference: a number is its uid (never a position),
/// `"#n"` the n-th group (1-based), any other string its name or match name.
pub(crate) fn pick_group<'a>(mut groups: impl Iterator<Item = &'a PropGroup>, v: &Value) -> Option<&'a PropGroup> {
    match v {
        Value::Number(n) => {
            let uid = n.as_u64()?;
            groups.find(|g| g.uid == uid)
        }
        Value::String(s) => match s.strip_prefix('#').and_then(|i| i.parse::<usize>().ok()) {
            Some(i) => groups.nth(i.checked_sub(1)?),
            None => {
                if let Some(uid) = s.strip_prefix('@').and_then(|u| u.parse::<u64>().ok()) {
                    return groups.find(|g| g.uid == uid);
                }
                let mut named = groups.filter(|g| &g.name == s || g.match_id == *s);
                let first = named.next()?;
                named.next().is_none().then_some(first)
            }
        },
        _ => None,
    }
}

/// `layer` param or the first selected layer.
pub(crate) fn layer_p(s: &Session, p: &Value, cmd: &str) -> Result<(ItemId, LayerId)> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    if let Some(v) = p.get("layer") {
        return checked_layer_ref(comp, cid, v, cmd)?.map(|l| (cid, l)).ok_or_else(|| bad(cmd, no_layer(v)));
    }
    s.state
        .selected_layers
        .first()
        .copied()
        .filter(|l| comp.layer(*l).is_some())
        .map(|l| (cid, l))
        .ok_or_else(|| bad(cmd, "no `layer` given and no layer selected"))
}

/// `layers` param (array) or the selection.
pub(crate) fn layers_p(s: &Session, p: &Value) -> Result<(ItemId, Vec<LayerId>)> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    // Explicit references that match no layer are an error (agents otherwise get a silent no-op).
    let resolve = |v: &Value| checked_layer_ref(comp, cid, v, "layers")?.ok_or_else(|| EngineError::Other(no_layer(v)));
    if let Some(Value::Array(a)) = p.get("layers") {
        return Ok((cid, a.iter().map(resolve).collect::<Result<_>>()?));
    }
    if let Some(v) = p.get("layer").filter(|v| !v.is_null()) {
        return Ok((cid, vec![resolve(v)?]));
    }
    Ok((cid, s.state.selected_layers.iter().copied().filter(|l| comp.layer(*l).is_some()).collect()))
}

/// Whether a layer can be picked by the user (Select All, Ctrl+Up/Down, a Timeline click): not
/// locked, and not hidden by the composition's Hide Shy Layers.
pub(crate) fn selectable(comp: &Comp, l: &Layer) -> bool {
    !(l.switches.locked || (comp.hide_shy && l.switches.shy))
}

/// The layers of `ids` a command may edit: locked layers are left alone, as in After Effects.
/// An error when every one of them is locked.
pub(crate) fn unlocked(s: &Session, cid: ItemId, ids: Vec<LayerId>, cmd: &str) -> Result<Vec<LayerId>> {
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let locked = |id: &LayerId| comp.layer(*id).is_some_and(|l| l.switches.locked);
    match ids.iter().find(|id| locked(id)) {
        Some(first) if ids.iter().all(locked) => {
            let name = comp.layer(*first).map(|l| l.name.clone()).unwrap_or_default();
            Err(bad(cmd, format!("“{name}” is locked (Layer ▸ Switches ▸ Unlock All Layers)")))
        }
        _ => Ok(ids.into_iter().filter(|id| !locked(id)).collect()),
    }
}

/// `visible: [{layer, prop}]`: the properties the Timeline shows (J / K and Select All
/// Keyframes use only those, as in After Effects). `None` when not given.
pub(crate) fn visible_p(p: &Value) -> Option<Vec<(LayerId, effectcraft_project::Uid)>> {
    let a = p.get("visible")?.as_array()?;
    Some(a.iter().filter_map(|v| Some((LayerId(v.get("layer")?.as_u64()?), v.get("prop")?.as_u64()?))).collect())
}

/// `time` (seconds) or `frame` param, else the CTI.
pub(crate) fn time_p(s: &Session, p: &Value, comp: Option<&Comp>) -> Tick {
    if let Some(t) = f_p(p, "time") {
        return Tick::from_seconds_f64(t);
    }
    if let (Some(f), Some(c)) = (p.get("frame").and_then(Value::as_i64), comp) {
        return c.frame_rate.tick_of(f);
    }
    s.time()
}

pub(crate) fn layer_mut(p: &mut effectcraft_project::Project, cid: ItemId, lid: LayerId) -> Result<&mut Layer> {
    p.comp_mut(cid).ok_or(EngineError::NoComp)?.layer_mut(lid).ok_or(EngineError::Project(effectcraft_project::ProjectError::NoLayer(lid)))
}
