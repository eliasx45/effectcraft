//! The tool catalogue: names, descriptions, JSON input schemas and implementations. Shared by the
//! MCP server (`tools/list`, `tools/call`) and the CLI's one-shot subcommands, so both behave the
//! same. Every tool maps onto engine commands (`Session::execute`) or control-channel methods.

use serde_json::{Map, Value, json};

use crate::backend::NEED_BRIDGE;
use crate::{Backend, Error, Result, encode_png, recompress_png};

/// A tool's output.
pub enum Reply {
    Json(Value),
    /// A PNG plus a JSON description of it (comp, time, size, path).
    Image {
        png: Vec<u8>,
        info: Value,
    },
}

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// Only listed (and usable) with a UI: bridged to the running app, or `mcp --ui` (offscreen).
    pub bridge_only: bool,
    pub(crate) schema: fn() -> Value,
    pub(crate) run: fn(&mut Backend, &Value) -> Result<Reply>,
}

impl ToolDef {
    /// The JSON Schema of the tool's arguments.
    pub fn input_schema(&self) -> Value {
        let mut schema = (self.schema)();
        crate::senses::widen_time_schema(&mut schema);
        if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            for (key, value) in [
                ("full", full_s("reply")),
                ("offset", offset_s()),
                (
                    "page_token",
                    json!({"type": "string", "description": "Paging: the `page_token` of the previous page; continues there (offset and the revision it was read at)."}),
                ),
                ("limit", limit_s("items or bytes", 100)),
                ("max_chars", max_chars_s()),
                (
                    "expectRevision",
                    json!({"type":"integer","minimum":0,"description":"Refuse if the project changed since this revision. Required for continuation of live inspection pages; saved get_result snapshots do not need it."}),
                ),
            ] {
                if key == "page_token" && !LIVE_PAGED.contains(&self.name) {
                    continue;
                }
                if key == "expectRevision"
                    && !(READ_ONLY.contains(&self.name)
                        || [
                            "set_property",
                            "add_keyframe",
                            "add_effect",
                            "run_script",
                            "command_run",
                            "execute_command",
                            "batch",
                            "command_batch",
                            "open_project",
                            "save_project",
                        ]
                        .contains(&self.name))
                {
                    continue;
                }
                props.entry(key).or_insert(value);
            }
        }
        schema
    }
    /// The MCP `Tool` object, with a title and annotations (docs/mcp.md).
    pub fn descriptor(&self) -> Value {
        let title = title_of(self.name);
        let read_only = READ_ONLY.contains(&self.name);
        let files = WRITES_FILES.contains(&self.name);
        json!({
            "name": self.name,
            "title": title,
            "description": self.description,
            "inputSchema": self.input_schema(),
            "annotations": {
                "title": title,
                "readOnlyHint": read_only,
                "destructiveHint": !read_only && !files,
                "idempotentHint": read_only || files,
                "openWorldHint": false,
            },
        })
    }

    /// Rename argument names agents mix up between tools to the ones this tool takes: `query` ↔
    /// `filter`, `source` / `script` → `code`, and camelCase ↔ snake_case (`maxSide` →
    /// `max_side`). Only when this tool doesn't take the given name itself and the call doesn't
    /// already pass the canonical one, so a real unknown argument is still reported.
    pub fn normalize_args(&self, args: &mut Value) {
        let Some(m) = args.as_object_mut() else { return };
        let schema = self.input_schema();
        let Some(props) = schema.get("properties").and_then(Value::as_object) else { return };
        let renames: Vec<(String, String)> = m
            .keys()
            .filter(|k| k.as_str() != "client" && !props.contains_key(*k))
            .filter_map(|k| arg_alias(k, props).filter(|c| !m.contains_key(c)).map(|c| (k.clone(), c)))
            .collect();
        for (from, to) in renames {
            if let Some(v) = m.remove(&from) {
                m.insert(to, v);
            }
        }
    }

    /// The first argument this tool doesn't take, as a message naming the accepted ones.
    pub fn unknown_arg(&self, args: &Value) -> Option<String> {
        let Some(m) = args.as_object() else { return Some("arguments must be an object".into()) };
        let schema = self.input_schema();
        let props = schema.get("properties")?.as_object()?;
        // `client` (who the agent is) goes with every tool: see `server::guarded_run`.
        let bad = m.keys().find(|k| k.as_str() != "client" && !props.contains_key(*k))?;
        let accepted: Vec<&str> = props.keys().map(String::as_str).collect();
        // `command` ↔ `id` is the usual mix-up between the command tools; else the closest name.
        let hint = match bad.as_str() {
            "command" if props.contains_key("id") => "; did you mean `id`?".to_string(),
            "id" if props.contains_key("command") => "; did you mean `command`?".to_string(),
            _ => effectcraft_engine::suggest::did_you_mean(bad, accepted.iter().copied()),
        };
        Some(format!("unknown argument \"{bad}\" for {}{hint}; expected: {}", self.name, accepted.join(", ")))
    }
}

/// Names agents use for the same argument across tools: (given, canonical).
const ARG_ALIASES: &[(&str, &str)] =
    &[("query", "filter"), ("search", "filter"), ("filter", "query"), ("search", "query"), ("source", "code"), ("script", "code"), ("js", "code")];

/// The argument of `props` that `key` stands for: a listed synonym, or the same name in the other
/// case style (`maxSide` ↔ `max_side`).
fn arg_alias(key: &str, props: &Map<String, Value>) -> Option<String> {
    if let Some((_, canonical)) = ARG_ALIASES.iter().find(|(given, canonical)| *given == key && props.contains_key(*canonical)) {
        return Some((*canonical).to_string());
    }
    let snake = key.chars().fold(String::new(), |mut s, c| {
        if c.is_ascii_uppercase() {
            s.push('_');
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
        s
    });
    let mut camel = String::new();
    let mut upper = false;
    for c in key.chars() {
        match c {
            '_' => upper = true,
            c if upper => {
                camel.push(c.to_ascii_uppercase());
                upper = false;
            }
            c => camel.push(c),
        }
    }
    [snake, camel].into_iter().find(|k| k != key && props.contains_key(k))
}

/// Tools that only read.
const READ_ONLY: &[&str] = &[
    "edit_feed",
    "taste_check",
    "ease_graph",
    "command_list",
    "list_commands",
    "describe_command",
    "doc_inspect",
    "get_project",
    "get_comp",
    "get_layer",
    "get_property",
    "get_field",
    "get_result",
    "list_effects",
    "list_fonts",
    "get_state",
    "ui_inspect",
    "render_times",
    "ui_elements",
    "get_layout",
    "lint_frame",
    "get_motion",
];
/// Tools that write files (or keep session-side frames) without changing the project.
const WRITES_FILES: &[&str] =
    &["save_project", "render_frame", "render_preview", "render_frames", "screenshot", "render_snapshot", "render_compare", "analyze_audio"];

/// Older tool names kept callable but left out of `tools/list` (`mcp --all-tools` lists them):
/// (alias, canonical tool). The canonical one takes every argument the alias does, except
/// `execute_command`'s `command` key (`command_run` takes `id`).
pub const HIDDEN_ALIASES: &[(&str, &str)] =
    &[("list_commands", "command_list"), ("execute_command", "command_run"), ("get_project", "doc_inspect"), ("render_frame", "render_preview")];

/// The canonical tool a hidden alias stands for.
pub fn alias_of(name: &str) -> Option<&'static str> {
    HIDDEN_ALIASES.iter().find(|(alias, _)| *alias == name).map(|(_, canonical)| *canonical)
}

/// Whether a tool can change the project (anything but queries and file writers).
pub fn may_edit(name: &str) -> bool {
    !READ_ONLY.contains(&name) && !WRITES_FILES.contains(&name)
}

/// `get_project` → "Get project".
fn title_of(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// All tools (headless + bridge-only).
pub fn catalogue() -> &'static [ToolDef] {
    TOOLS
}

pub fn find(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

/// The tools available on a backend (UI tools are hidden in headless mode without a UI).
pub fn available(bridge: bool) -> impl Iterator<Item = &'static ToolDef> {
    TOOLS.iter().filter(move |t| bridge || !t.bridge_only)
}

/// The JSON Schema of a tool's `structuredContent` (MCP 2025-06-18 `outputSchema`), for tools
/// whose result shape is stable. Clients that negotiated 2025-06-18+ get every JSON result as
/// `structuredContent` too ([`structured`]); only these declare its shape.
pub fn output_schema(name: &str) -> Option<Value> {
    let object = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
    let int = json!({"type": "integer"});
    let legacy = match name {
        "get_state" => object(json!({"state": {"type": "object"}, "app": {"type": "object"}}), &["state", "app"]),
        "doc_inspect" => object(json!({"project": {"type": "object"}, "activeComp": {"type": ["object", "null"]}}), &["project", "activeComp"]),
        "render_preview" | "render_frame" => object(
            json!({
                "comp": {"type": ["integer", "string", "null"], "description": "Composition id (null for an `item` render)."},
                "item": {"type": "integer", "description": "The footage or solid item rendered (with `item`)."},
                "layer": {"type": "integer", "description": "The soloed layer (with `layer`)."},
                "time": {"type": "number", "description": "Comp time in seconds."},
                "frame": int,
                "width": int, "height": int,
                "scale": {"type": "number", "description": "Output pixels per comp pixel."},
                "region": {"type": "array", "items": {"type": "number"}, "description": "The rendered [x, y, w, h] (with `region`)."},
                "overlays": {"type": "array", "items": {"type": "string"}},
                "layers": {"type": "array", "items": {"type": "object"}, "description": "Drawn layers with comp-space `bounds` and `anchor` (with bounds / anchor / motionPath overlays)."},
                "path": {"type": "string", "description": "Where the PNG was written (with `path`)."}
            }),
            &["comp", "time", "width", "height"],
        ),
        "render_frames" => object(
            json!({
                "times": {"type": "array", "items": {"type": "number"}}, "frames": {"type": "array", "items": int},
                "count": int, "requested": int, "cols": int, "rows": int,
                "cell": {"type": "array", "items": int}, "width": int, "height": int,
                "path": {"type": "string"}, "gif": {"type": "string"}
            }),
            &["times", "frames", "count", "cols", "rows", "cell", "width", "height"],
        ),
        "render_snapshot" => object(
            json!({
                "name": {"type": "string"}, "comp": int, "time": {"type": "number", "description": "Comp time in seconds."},
                "width": int, "height": int,
                "snapshots": {"type": "array", "items": {"type": "string"}, "description": "Kept snapshot names, oldest first."},
                "evicted": {"type": "array", "items": {"type": "string"}, "description": "Snapshots dropped to make room."}
            }),
            &["name", "comp", "time", "width", "height", "snapshots"],
        ),
        "render_compare" => {
            let num = json!({"type": "number"});
            let rect = json!({"type": "array", "items": int, "minItems": 4, "maxItems": 4});
            object(
                json!({
                    "width": int, "height": int,
                    "max_abs": num, "mean_abs": num, "alpha_max_abs": num,
                    "psnr_db": {"type": ["number", "string"], "description": "dB, or \"inf\" when the colour is identical."},
                    "ssim": num, "changed_pixels": int, "changed_pct": num, "threshold": num, "pass": {"type": "boolean"},
                    "diffBounds": {"type": ["array", "null"], "items": int, "minItems": 4, "maxItems": 4, "description": "[x, y, w, h] of the pixels over the threshold, in compared-frame pixels; null when none."},
                    "region": rect, "min_psnr": num,
                    "a": {"type": "object"}, "b": {"type": "object"},
                    "resampled": {"type": "string", "enum": ["a", "b"]}, "sizes": {"type": "object"},
                    "diff_path": {"type": "string"}, "diff_image": {"type": "array", "items": int, "description": "Size of the inline heatmap image."}
                }),
                &[
                    "width",
                    "height",
                    "max_abs",
                    "mean_abs",
                    "alpha_max_abs",
                    "psnr_db",
                    "ssim",
                    "changed_pixels",
                    "changed_pct",
                    "threshold",
                    "pass",
                    "diffBounds",
                    "a",
                    "b",
                ],
            )
        }
        "undo" | "redo" => object(json!({"steps": int, "undoStack": {"type": "array", "items": {"type": "string"}}}), &["steps", "undoStack"]),
        "history" => {
            object(json!({"states": {"type": "array", "items": {"type": "object"}}, "current": {"type": ["integer", "null"]}, "branches": int}), &["states"])
        }
        "command_batch" => object(
            json!({
                "completed": int, "failed": int,
                "results": {"type": "array", "items": object(json!({"ok": {"type": "boolean"}, "result": {}, "error": {"type": "string"}, "omitted": {"type": "boolean"}}), &["ok"])},
                "failedSteps": {"type": "array", "items": object(json!({"step": int, "id": {"type": "string"}, "error": {"type": "string"}}), &["step", "error"])}
            }),
            &["completed", "failed", "results"],
        ),
        "batch" => object(json!({"steps": int, "results": {"type": "array"}}), &["steps", "results"]),
        "save_project" => object(json!({"path": {"type": ["string", "null"]}, "dirty": {"type": "boolean"}}), &["path", "dirty"]),
        "get_layout" => object(
            json!({
                "comp": int, "time": {"type": "number"}, "width": int, "height": int,
                "layers": {"type": "array", "items": object(json!({"id": int, "name": {"type": "string"}, "visible": {"type": "boolean"}, "bounds": {"type": ["array", "null"]}}), &["id", "visible"])},
                "overlaps": {"type": "array", "items": {"type": "object"}}
            }),
            &["comp", "time", "width", "height", "layers"],
        ),
        "lint_frame" => {
            let problem = object(
                json!({
                    "rule": {"type": "string", "enum": effectcraft_engine::lint::RULES},
                    "severity": {"type": "string", "enum": ["error", "warning", "info"]},
                    "layer": int, "layerName": {"type": "string"},
                    "time": {"type": "number", "description": "First sample time it occurs at (comp seconds)."},
                    "until": {"type": "number", "description": "Last sample time it occurs at (comp seconds)."},
                    "message": {"type": "string"},
                    "other": {"type": "integer", "description": "The second layer of a pair (text-overlap, the cover of hidden-visible)."},
                    "otherName": {"type": "string"}
                }),
                &["rule", "severity", "layer", "layerName", "time", "until", "message"],
            );
            object(
                json!({
                    "comp": int, "compName": {"type": "string"},
                    "times": {"type": "array", "items": {"type": "number"}}, "truncated": {"type": "boolean"},
                    "clean": {"type": "boolean", "description": "No errors or warnings (info doesn't count)."},
                    "counts": object(json!({"error": int, "warning": int, "info": int}), &["error", "warning", "info"]),
                    "rules": {"type": "array", "items": {"type": "string"}},
                    "problems": {"type": "array", "items": problem},
                    "notes": {"type": "array", "items": {"type": "string"}},
                    "summary": {"type": "boolean", "description": "A summary: `problems` holds only the worst ones (errors first, no info)."},
                    "byRule": {"type": "object", "description": "Summary: problems per rule, by severity."},
                    "byLayer": {"type": "array", "items": object(json!({"layer": int, "layerName": {"type": "string"}, "problems": int, "rules": {"type": "array", "items": {"type": "string"}}}), &["layer", "problems", "rules"])},
                    "total": int, "listed": int
                }),
                &["comp", "compName", "times", "truncated", "clean", "counts", "problems"],
            )
        }
        "get_motion" => {
            let num = json!({"type": "number"});
            let seg = object(
                json!({"kind": {"type": "string", "enum": ["enter", "exit", "move", "hold", "scale", "rotate", "fade-in", "fade-out"]}, "start": num, "end": num, "from": {}, "to": {}, "direction": {"type": "string"}, "edge": {"type": "string"}}),
                &["kind", "start", "end"],
            );
            let span = object(json!({"start": num, "end": num}), &["start", "end"]);
            object(
                json!({
                    "comp": int, "compName": {"type": "string"}, "start": num, "end": num, "step": num, "truncated": {"type": "boolean"},
                    "layers": {"type": "array", "items": object(json!({"layer": int, "name": {"type": "string"}, "inPoint": num, "outPoint": num, "segments": {"type": "array", "items": seg}, "offscreen": {"type": "array", "items": span}, "summary": {"type": "string"}}), &["layer", "name", "segments", "offscreen", "summary"])},
                    "overlaps": {"type": "array", "items": object(json!({"a": int, "b": int, "aName": {"type": "string"}, "bName": {"type": "string"}, "start": num, "end": num, "maxFraction": num}), &["a", "b", "start", "end", "maxFraction"])}
                }),
                &["comp", "start", "end", "step", "truncated", "layers", "overlaps"],
            )
        }
        "analyze_audio" => {
            let db = json!({"type": ["number", "null"]});
            object(
                json!({
                    "comp": int, "start": {"type": "number"}, "end": {"type": "number"}, "hasAudio": {"type": "boolean"},
                    "peakDbfs": db, "truePeakDbtp": db, "rmsDbfs": db, "integratedLufs": db,
                    "clippedSamples": int, "silences": {"type": "array", "items": {"type": "object"}}, "channels": {"type": "array", "items": {"type": "object"}}
                }),
                &["comp", "start", "end", "hasAudio"],
            )
        }
        _ => return None,
    };
    // Bounds may replace any payload field. Apply the legacy contract only to its own variant.
    Some(json!({
        "type": "object",
        "anyOf": [
            legacy,
            object(json!({"result_id": {"type": "string"}}), &["result_id"]),
            object(json!({
                "result_retained": {"type": "boolean", "const": false},
                "operation_completed": {"type": "boolean"}
            }), &["result_retained", "operation_completed"])
        ]
    }))
}

/// A JSON result as MCP `structuredContent`, which must be an object: arrays become
/// `{"items": [...]}`, other values `{"value": v}`.
pub fn structured(v: Value) -> Value {
    match v {
        Value::Object(_) => v,
        Value::Array(_) => json!({"items": v}),
        other => json!({"value": other}),
    }
}
/// Tools refused while file access is confined: they drive the live app like a person (menus,
/// shortcuts, Open Recent…) or call control-channel methods directly, past the command checks.
const UNCONFINED_ONLY: &[&str] = &["control", "ui_click", "ui_drag", "ui_key", "ui_type", "ui_set", "script_ui"];

/// Run a tool by name.
pub fn run(b: &mut Backend, name: &str, args: &Value) -> Result<Reply> {
    let t = find(name).ok_or_else(|| Error::BadArgs(format!("unknown tool `{name}`")))?;
    if t.bridge_only && !b.is_bridge() {
        return Err(Error::BadArgs(NEED_BRIDGE.into()));
    }
    if b.confinement().is_some() && UNCONFINED_ONLY.contains(&name) {
        return Err(Error::BadArgs(format!("`{name}` is not available while file access is confined (--automation-read-root/--automation-write-root)")));
    }
    let mut normalized = if args.is_null() { json!({}) } else { args.clone() };
    t.normalize_args(&mut normalized);
    resume_page(name, &mut normalized)?;
    let args = &normalized;
    // Validate delivery options before a script/edit can run.
    let delivery_budget = crate::compact::budget(args)?;
    let output_page = crate::compact::Page::from_args(args, 100)?;
    let expected = match get(args, "expectRevision") {
        Some(value) => Some(value.as_u64().ok_or_else(|| Error::BadArgs("expectRevision must be a non-negative integer".into()))?),
        None => None,
    };
    if LIVE_PAGED.contains(&name) && output_page.offset > 0 && expected.is_none() {
        return Err(Error::BadArgs(
            "live page continuation needs the first reply's `page_token` (or its `revision` as expectRevision); or use a retained get_result snapshot".into(),
        ));
    }
    // Generic output options must not leak into raw UI/control arguments.
    let native = (t.schema)();
    let mut run_args = args.clone();
    for key in ["full", "offset", "limit", "max_chars", "expectRevision"] {
        if native["properties"].get(key).is_none() {
            // Convenience edit functions consume expectRevision for their root transaction.
            if key == "expectRevision" {
                continue;
            }
            if let Some(o) = run_args.as_object_mut() {
                o.remove(key);
            }
        }
    }
    // Timecode / frame times ("6:20", {"frame": 200}) become seconds before a tool checks them.
    let run_args = crate::senses::resolve_times(b, run_args)?;
    let outer = b.set_expected_revision(expected);
    let result = (t.run)(b, &run_args);
    b.set_expected_revision(outer);
    let mut reply = match result {
        Ok(reply) => reply,
        Err(error) => {
            let text = error.to_string();
            if text.len() <= delivery_budget || (get(args, "full").and_then(Value::as_bool) == Some(true) && !crate::compact::Page::requested(args)) {
                return Err(error);
            }
            Reply::Json(json!({"ok":false,"error":text,"outcome":"failed_or_unknown","operation_completed":!matches!(error,Error::Bridge(_))}))
        }
    };
    let writes = match name {
        "command_run" | "execute_command" => get(args, "id")
            .or_else(|| get(args, "command"))
            .and_then(Value::as_str)
            .and_then(effectcraft_engine::commands::find)
            .is_some_and(|c| c.journal || c.id.starts_with("edit.") || c.id == "engine.batch"),
        // A script's reply carries its own outcome; the session context only when asked.
        "run_script" => get(args, "context").and_then(Value::as_bool) == Some(true),
        _ => !READ_ONLY.contains(&name),
    };
    if writes
        && let Reply::Json(value) = &mut reply
        && value.is_object()
    {
        match b.observed_context() {
            Ok(context) => value["observedContext"] = context,
            Err(e) => value["contextWarning"] = json!(format!("operation already finished; could not observe current session: {e}; inspect before retrying")),
        }
    }
    crate::senses::attach_notices(b, name, &mut reply);
    if LIVE_PAGED.contains(&name)
        && let Reply::Json(value) = &mut reply
    {
        add_page_token(value);
    }
    b.finish_reply(name, args, reply)
}

/// Inspection tools that read the live project page by page: a continuation must come from the
/// same revision as the first page (else rows shift under the reader).
const LIVE_PAGED: &[&str] = &["get_comp", "get_layer", "get_property", "get_project", "doc_inspect", "get_field"];

/// A cut live reply gets a `page_token` for its next page: the revision it was read at and the
/// next offset, so continuing needs nothing else (`expectRevision` comes with it).
fn add_page_token(value: &mut Value) {
    let (Some(rev), Some(next)) = (value.get("revision").and_then(Value::as_u64), value.get("next_offset").and_then(Value::as_u64)) else { return };
    let token = format!("r{rev}o{next}");
    if let Some(t) = value.get_mut("truncated").filter(|t| t.is_object()) {
        t["hint"] = json!(format!("call again with page_token={token:?} (offset={next} at revision {rev})"));
    }
    value["page_token"] = json!(token);
}

/// `page_token` ("r<revision>o<offset>") into `offset` and, for live reads, `expectRevision`.
fn resume_page(name: &str, args: &mut Value) -> Result<()> {
    let Some(o) = args.as_object_mut() else { return Ok(()) };
    let Some(token) = o.remove("page_token").filter(|t| !t.is_null()) else { return Ok(()) };
    let bad = || Error::BadArgs(format!("`page_token` {token} is not one from a reply (r<revision>o<offset>)"));
    let (rev, offset) = token.as_str().and_then(|t| t.strip_prefix('r')).and_then(|t| t.split_once('o')).ok_or_else(bad)?;
    let (rev, offset) = (rev.parse::<u64>().map_err(|_| bad())?, offset.parse::<u64>().map_err(|_| bad())?);
    o.insert("offset".into(), json!(offset));
    if LIVE_PAGED.contains(&name) {
        o.entry("expectRevision").or_insert(json!(rev));
    }
    Ok(())
}

// ------------------------------------------------------------------ schema fragments

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}
fn offset_s() -> Value {
    json!({"type": "integer", "minimum": 0, "description": "Paging: index of the first item to return (default 0). Use the reply's `next_offset` to continue."})
}
fn limit_s(what: &str, default: usize) -> Value {
    json!({"type": "integer", "minimum": 1, "description": format!("Paging: at most this many {what} (default {default}). A cut list says so in `truncated` and `next_offset`.")})
}
fn max_chars_s() -> Value {
    json!({"type": "integer", "minimum": 1024, "description": "Maximum serialized reply size (default 7000). Large fields return an explicit get_field pointer; fetch their exact JSON in chunks before editing."})
}
fn full_s(what: &str) -> Value {
    json!({"type": "boolean", "description": format!("true: {what} with every field (the long form) instead of the compact default.")})
}
fn comp_s() -> Value {
    json!({"type": ["integer", "string"], "description": "Composition item id or name; omit for the active comp."})
}
fn layer_s() -> Value {
    json!({"type": ["integer", "string"], "description": "Layer id (from get_comp), \"#n\" (1-based index from the top) or layer name."})
}
fn path_s() -> Value {
    json!({"type": "string", "description": "Property path from get_layer: match ids joined by `/`, `#n` for the nth child, or `@uid`. E.g. `transform/position`, `transform/opacity`, `transform/scale`, `transform/rotation`, `effects/#1/blurriness`, `masks/#1/feather`, `@57`."})
}
fn item_s() -> Value {
    json!({"type": ["integer", "string"], "description": "Render a project item on its own instead of a comp: a footage or solid item (id or name), at source time `time`; a comp item is the same as `comp`."})
}
fn solo_s() -> Value {
    json!({"type": ["integer", "string"], "description": "Render only this layer (solo, in its place in the comp): id, \"#n\" or name."})
}
fn region_s() -> Value {
    json!({"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4, "description": "[x, y, w, h] in comp pixels: render just this rectangle at full resolution (clipped to the frame; `max_side` then applies to it). For detail checks on big comps."})
}
fn overlays_s() -> Value {
    json!({"type": ["boolean", "array", "string"], "items": {"type": "string", "enum": ["bounds", "anchor", "motionPath", "safeMargins", "grid", "all"]}, "description": "Draw the viewer's overlays on top: `bounds` (layer boxes tagged `#n name`, in label colours), `anchor`, `motionPath` (anchor path with keyframe squares), `safeMargins`, `grid`, or `all`; true = bounds + anchor. The reply's `layers` lists each drawn layer's comp-space `bounds` and `anchor`."})
}
fn value_s() -> Value {
    json!({"description": "Number for 1-D properties (50), array for vectors ([960,540]), \"#rrggbb\" or [r,g,b,a] (0-1) for colours, bool for checkboxes, string for text/menus."})
}

// ------------------------------------------------------------------ helpers

/// Build a params object from the present, non-null entries.
fn obj(entries: &[(&str, Option<&Value>)]) -> Value {
    let mut m = Map::new();
    for (k, v) in entries {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            m.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

fn get<'a>(a: &'a Value, k: &str) -> Option<&'a Value> {
    a.get(k).filter(|v| !v.is_null())
}

fn need<'a>(a: &'a Value, k: &str) -> Result<&'a Value> {
    get(a, k).ok_or_else(|| Error::BadArgs(format!("missing `{k}`")))
}

fn json_reply(v: Value) -> Result<Reply> {
    Ok(Reply::Json(v))
}

/// Accept params given as a JSON string too (some clients stringify nested objects).
fn params_of(v: Option<&Value>) -> Result<Value> {
    match v {
        None => Ok(json!({})),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(json!({})),
        Some(Value::String(s)) => serde_json::from_str(s).map_err(|e| Error::BadArgs(format!("`params` is not valid JSON: {e}"))),
        Some(v) => Ok(v.clone()),
    }
}

// ------------------------------------------------------------------ engine tools

fn list_commands(b: &mut Backend, a: &Value) -> Result<Reply> {
    let schemas = get(a, "schemas").and_then(Value::as_bool).unwrap_or(false).then_some(json!(true));
    let r = b.exec(
        "command.list",
        obj(&[("filter", get(a, "filter")), ("enabledOnly", get(a, "enabled_only")), ("schemas", schemas.as_ref()), ("brief", get(a, "brief"))]),
    )?;
    // Drop empty fields to keep the listing compact.
    let list: Vec<Value> = r
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut c| {
            if let Some(o) = c.as_object_mut() {
                o.retain(|_, v| !(v.is_null() || v.as_array().is_some_and(Vec::is_empty) || v.as_str() == Some("{}")));
            }
            c
        })
        .collect();
    json_reply(json!(list))
}

fn execute_command(b: &mut Backend, a: &Value) -> Result<Reply> {
    let id = get(a, "command").or(get(a, "id")).and_then(Value::as_str).ok_or_else(|| Error::BadArgs("missing `command` (e.g. `comp.new`)".into()))?;
    let params = params_of(get(a, "params"))?;
    json_reply(b.exec(id, params)?)
}

fn describe_command(b: &mut Backend, a: &Value) -> Result<Reply> {
    let id = get(a, "command").or(get(a, "id")).and_then(Value::as_str).ok_or_else(|| Error::BadArgs("missing `command` (e.g. `layer.newText`)".into()))?;
    json_reply(b.exec("command.describe", json!({"command": id}))?)
}

fn list_effects(b: &mut Backend, a: &Value) -> Result<Reply> {
    json_reply(b.exec("effect.list", obj(&[("filter", get(a, "filter")), ("brief", get(a, "brief"))]))?)
}

fn list_fonts(b: &mut Backend, a: &Value) -> Result<Reply> {
    json_reply(b.exec("text.fonts", obj(&[("query", get(a, "query")), ("variable", get(a, "variable")), ("rescan", get(a, "rescan"))]))?)
}

/// `effect.apply` on one layer, then set parameters on the new instance by param id: one batch,
/// so one undo step, and a failing value leaves nothing applied.
fn add_effect(b: &mut Backend, a: &Value) -> Result<Reply> {
    let (layer, effect, comp) = (need(a, "layer")?, need(a, "effect")?, get(a, "comp"));
    let count = |t: &Value| effects_group(t).and_then(|g| g.get("children")).and_then(Value::as_array).map_or(0, Vec::len);
    let before = b.exec("layer.tree", obj(&[("layer", Some(layer)), ("comp", comp)]))?;
    let index = count(&before).saturating_add(1);
    let mut p = obj(&[("effect", Some(effect)), ("comp", comp)]);
    p["layers"] = json!([layer]);
    let mut steps = vec![json!({"command": "effect.apply", "params": p})];
    let prefix = format!("effects/#{index}");
    let mut set = vec![];
    if let Some(vals) = get(a, "values").and_then(Value::as_object) {
        for (k, v) in vals {
            let path = format!("{prefix}/{k}");
            steps.push(
                json!({"command": "prop.set", "params": obj(&[("layer", Some(layer)), ("comp", comp), ("path", Some(&json!(path))), ("value", Some(v))])}),
            );
            set.push(path);
        }
    }
    // The undo step is named like effect.apply's own ("Apply Gaussian Blur").
    let name = effect.as_str().map(|e| effectcraft_engine::effects::lookup(e).map_or(e, |s| s.name));
    let label = name.map(|n| json!(format!("Apply {n}")));
    let keys: Vec<&String> = get(a, "values").and_then(Value::as_object).map(|v| v.keys().collect()).unwrap_or_default();
    steps.push(json!({"command": "layer.tree", "params": {"layer": before["id"], "comp": before["comp"]}}));
    let done = b
        .exec("engine.batch", obj(&[("steps", Some(&json!(steps))), ("label", label.as_ref()), ("expectRevision", before.get("revision"))]))
        .map_err(|e| add_effect_error(e, &keys))?;
    let warnings: Vec<Value> = done["results"].as_array().map(|r| r.iter().flat_map(warnings_of).collect()).unwrap_or_default();
    let tree = done
        .get("results")
        .and_then(Value::as_array)
        .and_then(|r| r.last())
        .cloned()
        .ok_or_else(|| Error::App("effect was applied but final inspection is missing; inspect before retrying".into()))?;
    let inst = effects_group(&tree).and_then(|g| g.get("children")).and_then(Value::as_array).and_then(|c| c.get(index - 1)).cloned();
    let mut params = vec![];
    if let Some(i) = &inst {
        flatten(i, &mut params);
    }
    let mut out = json!({"comp": tree["comp"], "revision": tree["revision"], "applied": true, "rolled_back": false, "layer": tree["id"], "index": index, "path": prefix, "name": inst.as_ref().map(|i| i["name"].clone()), "set": set, "params": params});
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    json_reply(out)
}

/// An add_effect failure in the words of its arguments: the batch's `step 3 (prop.set): invalid
/// parameters for `prop.set`: …` becomes `values.inBlack: …` (step 1 is the effect itself).
fn add_effect_error(e: Error, keys: &[&String]) -> Error {
    let msg = e.to_string();
    let Some((head, rest)) = msg.strip_prefix("step ").and_then(|m| m.split_once("): ")) else { return e };
    let Some(n) = head.split_whitespace().next().and_then(|n| n.parse::<usize>().ok()) else { return e };
    let rest = rest
        .strip_suffix("; the batch was rolled back (nothing changed)")
        .or_else(|| rest.strip_suffix("; the batch was rolled back"))
        .map_or(rest.to_string(), |r| format!("{r} (nothing was applied)"));
    let inner = ["prop.set", "effect.apply"].iter().find_map(|c| rest.strip_prefix(&format!("invalid parameters for `{c}`: "))).unwrap_or(&rest).to_string();
    match n.checked_sub(2).and_then(|i| keys.get(i)) {
        Some(k) => Error::BadArgs(format!("values.{k}: {inner}")),
        None => Error::BadArgs(inner),
    }
}

/// The `warnings` of a command result (prop.set / prop.addKey report clamped or coerced values).
fn warnings_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Object(o) => {
            let mut warnings = o.get("warnings").and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(count) = o.get("count") {
                warnings.extend(warnings_of(count));
            }
            warnings
        }
        Value::Array(a) => a.iter().flat_map(warnings_of).collect(),
        _ => vec![],
    }
}

/// The `effects` group of a `layer.tree` reply.
fn effects_group(tree: &Value) -> Option<&Value> {
    tree["properties"]["children"].as_array()?.iter().find(|c| c["path"] == "effects")
}

fn get_state(b: &mut Backend, _: &Value) -> Result<Reply> {
    let state = b.exec("editor.state", json!({}))?;
    let caps = b.exec("app.capabilities", json!({}))?;
    json_reply(
        json!({"state": state, "app": caps, "cliBuild": effectcraft_engine::build_info::build(), "protocolFeatures": effectcraft_engine::build_info::protocol_features()}),
    )
}

fn run_script(b: &mut Backend, a: &Value) -> Result<Reply> {
    // `file`: a script file on this machine. Its code runs (unless `code` is given) under its path,
    // so `#include "x.jsx"` resolves next to it rather than in the project's folder.
    let file = get(a, "file").and_then(Value::as_str);
    let code = match (get(a, "code").and_then(Value::as_str), file) {
        (Some(code), _) => code.to_string(),
        (None, Some(_)) if b.confinement().is_some() => return Err(Error::BadArgs("`file` is not available while file access is confined".into())),
        (None, Some(f)) => std::fs::read_to_string(f).map_err(|e| Error::BadArgs(format!("cannot read script file {f}: {e}")))?,
        (None, None) => return Err(Error::BadArgs("missing `code` (JavaScript) or `file` (a .jsx path)".into())),
    };
    let mut p = json!({"code": code});
    if let Some(n) = get(a, "name").or(file.map(|_| &a["file"])) {
        p["name"] = n.clone();
    }
    if let Some(r) = get(a, "rollback") {
        p["rollback"] = r.clone();
    }
    // A time limit by default, so a runaway loop can't hold the session forever; 0 = none.
    p["timeout"] = get(a, "timeout").cloned().unwrap_or(json!(SCRIPT_TIMEOUT));
    json_reply(b.exec("script.run", p)?)
}

/// The default `run_script` time limit, in seconds.
pub const SCRIPT_TIMEOUT: f64 = 300.0;

fn get_project(b: &mut Backend, _: &Value) -> Result<Reply> {
    json_reply(b.exec("project.summary", json!({}))?)
}

fn get_comp(b: &mut Backend, a: &Value) -> Result<Reply> {
    let info = b.exec("comp.info", obj(&[("comp", get(a, "comp"))]))?;
    if get(a, "summary").and_then(Value::as_bool) == Some(true) {
        return json_reply(comp_summary(info));
    }
    json_reply(crate::compact::comp_reply(info, a)?)
}

/// Layers a `get_comp` summary lists.
const COMP_SUMMARY_LAYERS: usize = 20;

/// A comp's settings with layer counts (per type, hidden, parented) and only its top layers.
fn comp_summary(mut info: Value) -> Value {
    let layers = match info.get_mut("layers").map(Value::take) {
        Some(Value::Array(v)) => v,
        _ => Vec::new(),
    };
    let mut types: Map<String, Value> = Map::new();
    for l in &layers {
        let t = l["type"].as_str().unwrap_or("?").to_string();
        let n = types.get(&t).and_then(Value::as_u64).unwrap_or(0);
        types.insert(t, json!(n + 1));
    }
    let hidden = layers.iter().filter(|l| l["switches"]["video"] == false).count();
    let parented = layers.iter().filter(|l| !l["parent"].is_null()).count();
    info["summary"] = json!(true);
    info["layerCount"] = json!(layers.len());
    info["types"] = Value::Object(types);
    info["hidden"] = json!(hidden);
    info["parented"] = json!(parented);
    info["layers"] = layers
        .iter()
        .take(COMP_SUMMARY_LAYERS)
        .map(|l| json!({"index": l["index"], "id": l["id"], "name": l["name"], "type": l["type"], "in": l["in"], "out": l["out"]}))
        .collect();
    info["hint"] = json!(format!("summary: counts and the top {COMP_SUMMARY_LAYERS} layers; leave `summary` out for every layer (paged)"));
    info
}

/// Flatten a `layer.tree` property tree into `[{path, type, value, keys?, expression?}]`.
fn flatten(node: &Value, out: &mut Vec<Value>) {
    // A group cut off by `depth` stays visible (instead of vanishing from the flat list).
    if node.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push(json!({"path": node["path"], "uid": node["uid"], "type": "group", "name": node["name"], "enabled": node["enabled"], "effectiveEnabled": node["effectiveEnabled"], "truncated": true}));
        return;
    }
    match node.get("children").and_then(Value::as_array) {
        Some(ch) => ch.iter().for_each(|c| flatten(c, out)),
        None => {
            let mut o = json!({"path": node["path"], "type": node["type"], "value": node["value"]});
            for key in
                ["uid", "enabled", "effectiveEnabled", "rawValue", "evaluatedValue", "expressionEnabled", "expressionEffectiveEnabled", "expressionError"]
            {
                if let Some(value) = node.get(key) {
                    o[key] = value.clone();
                }
            }
            if let Some(k) = node.get("keys").and_then(Value::as_array) {
                o["keys"] = json!(k.len());
            }
            if let Some(e) = node.get("expression") {
                o["expression"] = e.clone();
            }
            out.push(o);
        }
    }
}

fn get_layer(b: &mut Backend, a: &Value) -> Result<Reply> {
    let p = obj(&[("layer", Some(need(a, "layer")?)), ("comp", get(a, "comp")), ("depth", get(a, "depth")), ("time", get(a, "time"))]);
    let mut r = b.exec("layer.tree", p)?;
    if get(a, "flat").and_then(Value::as_bool) == Some(true) {
        let mut props = vec![];
        flatten(&r["properties"], &mut props);
        crate::compact::flat_reply(&mut r, props, a)?;
    } else {
        r = crate::compact::bounded_reply(r, a)?;
    }
    json_reply(r)
}

fn get_property(b: &mut Backend, a: &Value) -> Result<Reply> {
    let p = obj(&[("layer", Some(need(a, "layer")?)), ("path", Some(need(a, "path")?)), ("comp", get(a, "comp")), ("time", get(a, "time"))]);
    json_reply(crate::compact::property_reply(b.exec("prop.get", p)?, a)?)
}

/// Fetch the exact JSON of one omitted field. Never execute a mutation to recover its output.
fn get_field(b: &mut Backend, a: &Value) -> Result<Reply> {
    let source = need(a, "source")?.as_str().ok_or_else(|| Error::BadArgs("`source` must name an inspection tool".into()))?;
    if matches!(source, "get_comp" | "get_layer" | "get_property") && get(a, "comp").is_none() {
        return Err(Error::BadArgs("get_field requires the original reply's explicit `comp` id".into()));
    }
    if matches!(source, "get_layer" | "get_property") && get(a, "time").is_none() {
        return Err(Error::BadArgs("get_field requires the original reply's explicit comp `time` to keep chunks stable".into()));
    }
    let raw = match source {
        "get_comp" => b.exec("comp.info", obj(&[("comp", get(a, "comp"))]))?,
        "get_property" => b.exec(
            "prop.get",
            obj(&[("comp", get(a, "comp")), ("layer", Some(need(a, "layer")?)), ("path", Some(need(a, "path")?)), ("time", get(a, "time"))]),
        )?,
        "get_layer" => {
            let mut r = b.exec(
                "layer.tree",
                obj(&[("comp", get(a, "comp")), ("layer", Some(need(a, "layer")?)), ("time", get(a, "time")), ("depth", get(a, "depth"))]),
            )?;
            if get(a, "flat").and_then(Value::as_bool) == Some(true) {
                let mut props = vec![];
                flatten(&r["properties"], &mut props);
                r["properties"] = json!(props);
            }
            r
        }
        "get_project" => b.exec("project.summary", json!({}))?,
        "doc_inspect" => {
            let project = b.exec("project.summary", json!({}))?;
            let comp_id = get(a, "comp").cloned().unwrap_or_else(|| project["activeComp"].clone());
            if get(a, "pointer").and_then(Value::as_str).is_some_and(|p| p.is_empty() || p.starts_with("/activeComp")) && get(a, "comp").is_none() {
                return Err(Error::BadArgs("doc_inspect field recovery requires the original activeComp id in `comp`".into()));
            }
            let comp = if comp_id.is_null() { Value::Null } else { b.exec("comp.info", json!({"comp": comp_id}))? };
            json!({"revision": project["revision"], "project": project, "activeComp": comp})
        }
        _ => return Err(Error::BadArgs("`source` must be get_comp, get_layer, get_property, get_project or doc_inspect".into())),
    };
    // The field helper adds read identity to its bounded chunk response.
    if raw.get("revision").is_none() {
        return Err(Error::App("this app build does not expose inspection revisions; update/reconnect it before fetching fields".into()));
    }
    let pointer = need(a, "pointer")?.as_str().ok_or_else(|| Error::BadArgs("`pointer` must be a JSON pointer string".into()))?;
    json_reply(crate::compact::field_chunk(&raw, pointer, a)?)
}

fn get_result(b: &mut Backend, a: &Value) -> Result<Reply> {
    json_reply(b.saved_result(a)?)
}

/// Atomic engine failures are known rollbacks; transport errors remain outcome-unknown.
fn atomic_edit(b: &mut Backend, a: &Value, label: &str, steps: Vec<Value>) -> Result<Value> {
    let mut params = json!({"steps": steps, "label": label, "atomic": true});
    if let Some(revision) = get(a, "expectRevision") {
        params["expectRevision"] = revision.clone();
    }
    match b.exec("engine.batch", params) {
        Ok(done) => Ok(done),
        Err(e) if e.to_string().contains("the batch was rolled back") => {
            Ok(json!({"ok": false, "error": e.to_string(), "applied": false, "rolled_back": true, "outcome": "rolled_back"}))
        }
        Err(e) => Err(e),
    }
}

fn mutation_reply(done: &Value) -> Result<Value> {
    let mut out = done
        .get("results")
        .and_then(Value::as_array)
        .and_then(|r| r.last())
        .cloned()
        .ok_or_else(|| Error::App("atomic edit returned no inspection; inspect the document before retrying".into()))?;
    let warnings: Vec<Value> = done.get("results").and_then(Value::as_array).into_iter().flatten().flat_map(warnings_of).collect();
    out["applied"] = json!(true);
    out["rolled_back"] = json!(false);
    out["outcome"] = json!("applied");
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

fn set_property(b: &mut Backend, a: &Value) -> Result<Reply> {
    let (layer, path, comp, time) = (need(a, "layer")?, need(a, "path")?, get(a, "comp"), get(a, "time"));
    let (value, expr) = (get(a, "value"), get(a, "expression"));
    if value.is_none() && expr.is_none() {
        return Err(Error::BadArgs("give `value` and/or `expression`".into()));
    }
    if expr.is_some_and(|v| !v.is_string()) {
        return Err(Error::BadArgs("`expression` must be a string; nothing was applied".into()));
    }
    reject_omitted_value(value)?;
    let inspect = obj(&[("layer", Some(layer)), ("path", Some(path)), ("comp", comp)]);
    let mut steps = vec![json!({"command": "prop.get", "params": inspect})];
    let base = json!({"comp": "$1.comp", "layer": "$1.layer", "prop": "$1.uid"});
    let mut timed_step = None;
    if let Some(value) = value {
        let mut p = base.clone();
        p["value"] = value.clone();
        let command = if let Some(time) = time {
            if !time.as_f64().is_some_and(f64::is_finite) {
                return Err(Error::BadArgs("`time` must be finite seconds; nothing was applied".into()));
            }
            p["time"] = time.clone();
            p["timeBase"] = get(a, "timeBase").cloned().unwrap_or(json!("layer"));
            timed_step = Some(steps.len() + 1);
            "prop.addKey"
        } else {
            "prop.set"
        };
        steps.push(json!({"command": command, "params": p}));
    }
    if let Some(expression) = expr {
        let mut p = base.clone();
        p["expression"] = expression.clone();
        steps.push(json!({"command": "prop.setExpression", "params": p}));
    }
    let mut read = base;
    if let Some(step) = timed_step {
        read["time"] = json!(format!("${step}.compTime"));
    } else if let Some(time) = time {
        read["time"] = time.clone();
    }
    steps.push(json!({"command": "prop.get", "params": read}));
    let done = atomic_edit(b, a, "Set Property", steps)?;
    if done.get("ok") == Some(&Value::Bool(false)) {
        return json_reply(done);
    }
    let mut out = mutation_reply(&done)?;
    if let Some(step) = timed_step {
        let timing = done
            .get("results")
            .and_then(Value::as_array)
            .and_then(|r| r.get(step - 1))
            .ok_or_else(|| Error::App("atomic edit returned no timing receipt; inspect before retrying".into()))?;
        out["requestedTime"] = time.cloned().unwrap_or(Value::Null);
        out["layerTime"] = timing["layerTime"].clone();
        out["compTime"] = timing["compTime"].clone();
        out["appliedValue"] = out["value"].clone();
        if let Some(w) = timing.get("warning") {
            out["timingWarning"] = w.clone();
        }
    }
    json_reply(out)
}

fn reject_omitted_value(value: Option<&Value>) -> Result<()> {
    fn omitted(v: &Value, depth: usize) -> bool {
        if depth > 64 {
            return true;
        }
        match v {
            Value::Object(o) => o.get("$omitted").and_then(Value::as_bool) == Some(true) || o.values().any(|v| omitted(v, depth + 1)),
            Value::Array(a) => a.iter().any(|v| omitted(v, depth + 1)),
            _ => false,
        }
    }
    if value.is_some_and(|v| omitted(v, 0)) {
        return Err(Error::BadArgs("value is omitted or too deeply nested; fetch exact data before editing; nothing was applied".into()));
    }
    Ok(())
}

fn add_keyframe(b: &mut Backend, a: &Value) -> Result<Reply> {
    let (layer, path, comp) = (need(a, "layer")?, need(a, "path")?, get(a, "comp"));
    let keys = match get(a, "keys") {
        Some(Value::Array(k)) if k.len() <= 1_000 => k.clone(),
        Some(Value::Array(_)) => return Err(Error::BadArgs("at most 1000 keys per atomic edit".into())),
        Some(_) => return Err(Error::BadArgs("`keys` must be an array of {time, value?}".into())),
        None => vec![obj(&[("time", Some(need(a, "time")?)), ("value", get(a, "value"))])],
    };
    if keys.is_empty() {
        return Err(Error::BadArgs("no keys".into()));
    }
    for (i, key) in keys.iter().enumerate() {
        if !key.get("time").and_then(Value::as_f64).is_some_and(f64::is_finite) {
            return Err(Error::BadArgs(format!("keys[{i}]: missing or invalid `time`; nothing was applied")));
        }
        reject_omitted_value(key.get("value"))?;
    }
    let interpolation = match get(a, "interpolation") {
        Some(Value::String(value)) => Some(value.to_ascii_lowercase()),
        Some(_) => return Err(Error::BadArgs("interpolation must be a string; nothing was applied".into())),
        None => None,
    };
    if interpolation.as_deref().is_some_and(|i| !["linear", "bezier", "hold", "easyease", "ease", "easyeasein", "easyeaseout"].contains(&i)) {
        return Err(Error::BadArgs("unknown interpolation; nothing was applied".into()));
    }
    let mut steps = vec![json!({"command": "prop.get", "params": obj(&[("layer", Some(layer)), ("path", Some(path)), ("comp", comp)])})];
    let base = json!({"comp": "$1.comp", "layer": "$1.layer", "prop": "$1.uid"});
    let mut selection = vec![];
    let mut last_step = 1;
    for key in &keys {
        let mut p = base.clone();
        p["time"] = key["time"].clone();
        p["timeBase"] = get(a, "timeBase").cloned().unwrap_or(json!("layer"));
        if let Some(value) = get(key, "value") {
            p["value"] = value.clone();
        }
        last_step = steps.len() + 1;
        steps.push(json!({"command": "prop.addKey", "params": p}));
        selection.push(json!({"layer": "$1.layer", "prop": "$1.uid", "time": format!("${last_step}.layerTime")}));
    }
    if let Some(interpolation) = interpolation {
        steps.push(json!({"command": "comp.open", "params": {"comp": "$1.comp"}}));
        steps.push(json!({"command": "keys.select", "params": {"keys": selection, "timeBase": "layer"}}));
        let (command, params) = match interpolation.as_str() {
            "easyease" | "ease" => ("keys.easyEase", json!({})),
            "easyeasein" => ("keys.easyEase", json!({"which": "in"})),
            "easyeaseout" => ("keys.easyEase", json!({"which": "out"})),
            other => ("keys.interpolation", json!({"interpolation": other})),
        };
        steps.push(json!({"command": command, "params": params}));
    }
    let mut read = base;
    read["time"] = json!(format!("${last_step}.compTime"));
    steps.push(json!({"command": "prop.get", "params": read}));
    let done = atomic_edit(b, a, "Add Keyframes", steps)?;
    if done.get("ok") == Some(&Value::Bool(false)) {
        return json_reply(done);
    }
    json_reply(mutation_reply(&done)?)
}

/// The render arguments shared by render_frame / render_preview / render_frames.
fn render_params(a: &Value, keys: &[&str]) -> Value {
    let entries: Vec<(&str, Option<&Value>)> = keys.iter().map(|k| (*k, get(a, k))).collect();
    obj(&entries)
}

const VIEW_KEYS: &[&str] = &["comp", "item", "layer", "time", "transparent", "region", "overlays"];

fn render_frame(b: &mut Backend, a: &Value) -> Result<Reply> {
    let mut p = render_params(a, VIEW_KEYS);
    p["max_side"] = get(a, "max_side").cloned().unwrap_or(json!(960));
    let (png, mut info) = b.render_view(p)?;
    if let Some(p) = get(a, "path").and_then(Value::as_str) {
        if let Some(c) = b.confinement() {
            c.check(p, crate::confine::Access::Write)?;
        }
        std::fs::write(p, &png).map_err(|e| Error::Other(format!("cannot write {p}: {e}")))?;
        info["path"] = json!(p);
    }
    if get(a, "inline").and_then(Value::as_bool) == Some(false) {
        return json_reply(info);
    }
    Ok(Reply::Image { png, info })
}

fn render_frames(b: &mut Backend, a: &Value) -> Result<Reply> {
    let mut p = render_params(
        a,
        &[
            "comp",
            "item",
            "layer",
            "times",
            "frames",
            "count",
            "start",
            "end",
            "cols",
            "max_side",
            "labels",
            "transparent",
            "region",
            "overlays",
            "path",
            "gif",
        ],
    );
    p["base64"] = json!(true);
    let mut r = b.exec("render.contactSheet", p)?;
    let png = take_png(&mut r, "render.contactSheet")?;
    if get(a, "inline").and_then(Value::as_bool) == Some(false) {
        return json_reply(r);
    }
    Ok(Reply::Image { png, info: r })
}

/// Remove and decode the base64 `png` of a render reply.
pub(crate) fn take_png(r: &mut Value, what: &str) -> Result<Vec<u8>> {
    let data = r.as_object_mut().and_then(|o| o.remove("png")).ok_or_else(|| Error::App(format!("{what} returned no `png`")))?;
    crate::base64::decode(data.as_str().unwrap_or_default()).ok_or_else(|| Error::App(format!("{what}: bad base64")))
}

fn render_snapshot(b: &mut Backend, a: &Value) -> Result<Reply> {
    let p = obj(&[
        ("name", get(a, "name")),
        ("comp", get(a, "comp")),
        ("time", get(a, "time")),
        ("max_side", get(a, "max_side")),
        ("transparent", get(a, "transparent")),
    ]);
    json_reply(b.exec("render.snapshot", p)?)
}

/// Longest side of the diff heatmap returned inline.
const DIFF_INLINE_SIDE: u32 = 768;

fn render_compare(b: &mut Backend, a: &Value) -> Result<Reply> {
    need(a, "a")?;
    need(a, "b")?;
    let inline = get(a, "inline").and_then(Value::as_bool) != Some(false);
    let mut p = obj(&[
        ("a", get(a, "a")),
        ("b", get(a, "b")),
        ("threshold", get(a, "threshold")),
        ("min_psnr", get(a, "min_psnr")),
        ("max_side", get(a, "max_side")),
        ("region", get(a, "region")),
        ("amplify", get(a, "amplify")),
        ("diff_path", get(a, "diff_path")),
    ]);
    if inline {
        p["diff_inline"] = json!(true);
    }
    let mut r = b.exec("render.compare", p)?;
    let diff = r.as_object_mut().and_then(|o| o.remove("diff_png"));
    // The heatmap is worth looking at only when something changed.
    let changed = r["changed_pixels"].as_u64().unwrap_or(0) > 0;
    match diff.as_ref().and_then(Value::as_str).filter(|_| changed) {
        Some(b64) => {
            let raw = crate::base64::decode(b64).ok_or_else(|| Error::App("render.compare: bad base64".into()))?;
            let (png, w, h) = recompress_png(&raw, DIFF_INLINE_SIDE)?;
            r["diff_image"] = json!([w, h]);
            Ok(Reply::Image { png, info: r })
        }
        None => json_reply(r),
    }
}

fn frame_source_s() -> Value {
    json!({
        "description": "A snapshot name (string), {\"snapshot\": name}, {\"path\": \"file.png\"} (PNG/JPEG, e.g. a golden or an After Effects reference render) or {\"comp\"?, \"time\"?, \"transparent\"?} to render now ({} = active comp at the current time).",
        "oneOf": [
            {"type": "string"},
            {"type": "object", "properties": {"snapshot": {"type": "string"}}, "required": ["snapshot"], "additionalProperties": false},
            {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"], "additionalProperties": false},
            {"type": "object", "properties": {"comp": comp_s(), "time": {"type": "number"}, "transparent": {"type": "boolean"}}, "additionalProperties": false}
        ]
    })
}

fn open_project(b: &mut Backend, a: &Value) -> Result<Reply> {
    // Replacing the open document: never silently drop another agent's or the user's unsaved
    // work on this host. `project.summary` (not `b.session()`, which is `None` over a bridge to
    // another process) reaches the dirty flag whether this server is headless or bridged.
    if get(a, "discard").and_then(Value::as_bool) != Some(true) && b.exec("project.summary", json!({}))?.get("dirty") == Some(&Value::Bool(true)) {
        return Err(Error::BadArgs("this host's project has unsaved changes: save_project first, or pass `discard: true` to drop them".into()));
    }
    let opened = if let Some(path) = get(a, "path") {
        b.exec("file.open", json!({"path": path}))?
    } else if get(a, "demo").and_then(Value::as_bool) == Some(true) {
        b.exec("file.openDemoProject", json!({}))?
    } else if get(a, "new").and_then(Value::as_bool) == Some(true) {
        b.exec("file.newProject", json!({}))?
    } else {
        return Err(Error::BadArgs("give `path` (.ecproj), `demo: true` or `new: true`".into()));
    };
    // The precondition guarded opening, not the deliberately new document revision.
    b.set_expected_revision(None);
    let mut project = match opened.get("project") {
        Some(p) => p.clone(),
        None => b.exec("project.summary", json!({}))?,
    };
    let mut warnings = vec![];
    for key in ["repaired", "savedBy", "handover", "openElsewhere"] {
        if let Some(value) = opened.get(key) {
            project[key] = value.clone();
        }
    }
    if opened.get("repaired").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        warnings.push(json!({"kind":"repairs", "details":opened["repaired"]}));
    }
    if opened.get("openElsewhere").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        warnings.push(json!({"kind":"openElsewhere", "hosts":opened["openElsewhere"]}));
    }
    if let Some(version) = opened.get("savedBy").and_then(Value::as_str)
        && effectcraft_engine::project::is_newer_version(version, effectcraft_engine::project::APP_VERSION)
    {
        warnings.push(json!({"kind":"newerVersion", "savedBy":version, "message":"Settings this build does not understand may be lost on save."}));
    }
    if get(a, "path").is_some() {
        // The existing headless notifier already waits for this check; surface it in-band too.
        let mut params = json!({"wait": true});
        if let Some(revision) = project.get("revision") {
            params["expectRevision"] = revision.clone();
        }
        match b.exec("footage.check", params) {
            Ok(check) => {
                if check.get("missing").and_then(Value::as_u64).is_some_and(|n| n>0) { warnings.push(json!({"kind":"missingMedia","details":check})); }
                project["mediaCheck"] = check;
            }
            Err(error) => warnings.push(json!({"kind":"mediaCheckUnavailable","message":error.to_string(),"messageDetail":"The project opened; media completeness has not been verified."})),
        }
    }
    if !warnings.is_empty() {
        project["warnings"] = json!(warnings);
    }
    json_reply(project)
}

/// `hosts`: where projects are open; `join` switches this server to one of them (a running app or
/// another host with a control port), keeping the agent's name so it works on its own cursor.
fn hosts(b: &mut Backend, a: &Value) -> Result<Reply> {
    let list = b.exec("hosts.list", json!({}))?;
    let Some(join) = get(a, "join") else { return json_reply(list) };
    let port = crate::host_join::select_port(&list, join)?;
    // Leaving a headless project: never drop unsaved work.
    if let Some(s) = b.session()
        && s.is_dirty()
    {
        return Err(Error::BadArgs("this server's project has unsaved changes: save_project first, then join".into()));
    }
    let mut next = Backend::bridge(&port.to_string())?;
    next.set_client(Some(b.client().map(str::to_string).unwrap_or_else(|| format!("mcp-{}", std::process::id()))));
    let summary = next.exec("project.summary", json!({}))?;
    next.set_expected_revision(summary.get("revision").and_then(Value::as_u64));
    let state = next.exec("editor.state", json!({}))?;
    next.set_expected_revision(None);
    crate::host_join::verify_connected_path(join, &state)?;
    let client = next.client().map(str::to_string);
    let mut identity_source = state.clone();
    identity_source["path"] = state["projectPath"].clone();
    *b = next;
    let context = crate::host_join::identity(Some(port), &json!({"kind": b.mode()}), &identity_source, client.as_deref());
    json_reply(json!({"joined": port, "client": client, "project": summary, "context": context}))
}

fn save_project(b: &mut Backend, a: &Value) -> Result<Reply> {
    let saved = match get(a, "path") {
        Some(path) => b.exec("file.saveAs", json!({"path":path}))?,
        None => b.exec("file.save", json!({}))?,
    };
    b.set_expected_revision(None);
    if saved.get("dirty").is_some() {
        return json_reply(saved);
    }
    let summary = b.exec("project.summary", json!({}))?;
    json_reply(json!({"path":saved["path"],"dirty":summary["dirty"],"revision":summary["revision"]}))
}

fn history(b: &mut Backend, a: &Value, id: &str) -> Result<Reply> {
    let steps = get(a, "steps").and_then(Value::as_u64).unwrap_or(1).max(1);
    let mut done = 0;
    for _ in 0..steps {
        match b.exec(id, json!({})) {
            Ok(_) => done += 1,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    let s = b.exec("project.summary", json!({}))?;
    json_reply(json!({"steps": done, "undoStack": s["undo"]}))
}

fn undo(b: &mut Backend, a: &Value) -> Result<Reply> {
    history(b, a, "edit.undo")
}

fn redo(b: &mut Backend, a: &Value) -> Result<Reply> {
    history(b, a, "edit.redo")
}

fn script_ui_tool(b: &mut Backend, a: &Value) -> Result<Reply> {
    let action = get(a, "action").and_then(Value::as_str).unwrap_or("list");
    let id = match action {
        "list" => "scriptui.list",
        "get" => "scriptui.get",
        "click" => "scriptui.click",
        "set" => "scriptui.set",
        "close" => "scriptui.close",
        other => return Err(Error::BadArgs(format!("unknown action `{other}` (list | get | click | set | close)"))),
    };
    let p = obj(&[("window", get(a, "window")), ("widget", get(a, "widget")), ("value", get(a, "value")), ("result", get(a, "result"))]);
    json_reply(b.exec(id, p)?)
}

fn history_tool(b: &mut Backend, a: &Value) -> Result<Reply> {
    if let Some(g) = get(a, "goto") {
        let p = match g {
            Value::String(id) if id.parse::<u64>().is_err() => json!({"id": id}),
            Value::String(n) => json!({"index": n.parse::<u64>().unwrap_or(0)}),
            v => json!({"index": v}),
        };
        b.exec("edit.history.goto", p)?;
        b.set_expected_revision(None);
    }
    json_reply(b.exec("edit.history.list", json!({}))?)
}

fn batch_tool(b: &mut Backend, a: &Value) -> Result<Reply> {
    let steps = need(a, "steps")?;
    let steps = match steps {
        Value::String(_) => params_of(Some(steps))?,
        v => v.clone(),
    };
    json_reply(b.exec("engine.batch", obj(&[("steps", Some(&steps)), ("label", get(a, "label")), ("atomic", get(a, "atomic"))]))?)
}

// ------------------------------------------------------------------ analysis tools

fn get_layout(b: &mut Backend, a: &Value) -> Result<Reply> {
    let layers = match get(a, "layers") {
        Some(Value::Array(v)) => Some(json!(v)),
        Some(v) => Some(json!([v])),
        None => None,
    };
    let p =
        obj(&[("comp", get(a, "comp")), ("time", get(a, "time")), ("layers", layers.as_ref()), ("pixels", get(a, "pixels")), ("overlaps", get(a, "overlaps"))]);
    json_reply(b.exec("layer.bounds", p)?)
}

fn lint_frame(b: &mut Backend, a: &Value) -> Result<Reply> {
    let rules = match get(a, "rules") {
        Some(Value::String(r)) => Some(json!([r])),
        other => other.cloned(),
    };
    let p = obj(&[
        ("comp", get(a, "comp")),
        ("time", get(a, "time")),
        ("start", get(a, "start")),
        ("end", get(a, "end")),
        ("step", get(a, "step")),
        ("rules", rules.as_ref()),
        ("minTextPx", get(a, "min_text_px")),
        ("minContrast", get(a, "min_contrast")),
    ]);
    let mut r = b.exec("render.lint", p)?;
    // Long lists come as a summary unless asked for in full (`summary: false`).
    let many = r["problems"].as_array().is_some_and(|v| v.len() > LINT_SUMMARY_OVER);
    if get(a, "summary").and_then(Value::as_bool).unwrap_or(many) {
        r = lint_summary(r);
    }
    json_reply(r)
}

/// `lint_frame` answers with a summary by default above this many problems.
const LINT_SUMMARY_OVER: usize = 40;
/// Problems and layers a summary lists.
const SUMMARY_TOP: usize = 10;

/// A lint reply cut to counts per rule and per layer plus the worst problems (errors first, info
/// left out).
fn lint_summary(mut r: Value) -> Value {
    let problems = r.get_mut("problems").map(Value::take).and_then(|v| match v {
        Value::Array(v) => Some(v),
        _ => None,
    });
    let problems = problems.unwrap_or_default();
    let rank = |p: &Value| match p["severity"].as_str() {
        Some("error") => 0,
        Some("warning") => 1,
        _ => 2,
    };
    let mut by_rule: Map<String, Value> = Map::new();
    let mut by_layer: Vec<(Value, Value, usize, Vec<String>)> = Vec::new();
    for p in &problems {
        let rule = p["rule"].as_str().unwrap_or("?").to_string();
        let sev = p["severity"].as_str().unwrap_or("info").to_string();
        let e = by_rule.entry(rule.clone()).or_insert_with(|| json!({}));
        e[&sev] = json!(e[&sev].as_u64().unwrap_or(0) + 1);
        if rank(p) < 2 {
            match by_layer.iter_mut().find(|l| l.0 == p["layer"]) {
                Some(l) => {
                    l.2 += 1;
                    if !l.3.contains(&rule) {
                        l.3.push(rule);
                    }
                }
                None => by_layer.push((p["layer"].clone(), p["layerName"].clone(), 1, vec![rule])),
            }
        }
    }
    by_layer.sort_by_key(|l| std::cmp::Reverse(l.2));
    let mut top: Vec<&Value> = problems.iter().filter(|p| rank(p) < 2).collect();
    top.sort_by_key(|p| rank(p));
    r["summary"] = json!(true);
    r["byRule"] = Value::Object(by_rule);
    r["byLayer"] = by_layer.iter().take(SUMMARY_TOP).map(|l| json!({"layer": l.0, "layerName": l.1, "problems": l.2, "rules": l.3})).collect();
    r["problems"] = top.iter().take(SUMMARY_TOP).map(|p| (*p).clone()).collect();
    r["listed"] = json!(top.len().min(SUMMARY_TOP));
    r["total"] = json!(problems.len());
    r["hint"] = json!("summary: the worst problems and counts per rule and layer; `summary: false` lists every problem, `rules` narrows to some rules");
    r
}

fn taste_check(b: &mut Backend, a: &Value) -> Result<Reply> {
    let p = obj(&[
        ("comp", get(a, "comp")),
        ("layer", get(a, "layer")),
        ("layers", get(a, "layers")),
        ("start", get(a, "start")),
        ("end", get(a, "end")),
        ("all", get(a, "all")),
        ("problemsOnly", get(a, "problems_only")),
    ]);
    json_reply(b.exec("taste.check", p)?)
}

fn ease_graph(b: &mut Backend, a: &Value) -> Result<Reply> {
    let keep = get(a, "path").and_then(Value::as_str).map(str::to_string);
    if let (Some(c), Some(p)) = (b.confinement(), &keep) {
        c.check(p, crate::confine::Access::Write)?;
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = keep
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("effectcraft-mcp-ease-{}-{n}.png", std::process::id())).to_string_lossy().into_owned());
    let mut p = obj(&[
        ("comp", get(a, "comp")),
        ("layer", get(a, "layer")),
        ("path", get(a, "property")),
        ("prop", get(a, "prop")),
        ("start", get(a, "start")),
        ("end", get(a, "end")),
        ("width", get(a, "width")),
        ("height", get(a, "height")),
    ]);
    p["out"] = json!(out);
    let mut info = b.exec("taste.graph", p)?;
    let png = std::fs::read(&out).map_err(|e| Error::Other(format!("ease_graph: {e}")))?;
    if keep.is_none() {
        let _ = std::fs::remove_file(&out);
        info["path"] = Value::Null;
    }
    Ok(Reply::Image { png, info })
}

fn get_motion(b: &mut Backend, a: &Value) -> Result<Reply> {
    let range = [("comp", get(a, "comp")), ("start", get(a, "start")), ("end", get(a, "end")), ("step", get(a, "step"))];
    if let Some(l) = get(a, "layer") {
        let mut p = obj(&range);
        p["layer"] = l.clone();
        return json_reply(b.exec("layer.motion", p)?);
    }
    let layers = match get(a, "layers") {
        Some(Value::Array(v)) => Some(json!(v)),
        Some(v) => Some(json!([v])),
        None => None,
    };
    let mut p = obj(&range);
    if let Some(l) = layers {
        p["layers"] = l;
    }
    json_reply(b.exec("comp.motion", p)?)
}

fn analyze_audio(b: &mut Backend, a: &Value) -> Result<Reply> {
    let waveform = get(a, "waveform").and_then(Value::as_bool).unwrap_or(false);
    let on = json!(true);
    let p = obj(&[
        ("comp", get(a, "comp")),
        ("start", get(a, "start")),
        ("end", get(a, "end")),
        ("layer", get(a, "layer")),
        ("silenceDb", get(a, "silence_db")),
        ("minSilence", get(a, "min_silence")),
        ("truePeak", get(a, "true_peak")),
        ("waveform", waveform.then_some(&on)),
        ("width", get(a, "width")),
        ("height", get(a, "height")),
        ("path", get(a, "path")),
    ]);
    let mut info = b.exec("audio.analyze", p)?;
    let Some(w) = info.get_mut("waveform").and_then(Value::as_object_mut) else { return json_reply(info) };
    // Draw the waveform here from its columns (headless or bridged alike) and drop them.
    let cols: Vec<[f32; 4]> = w
        .remove("columns")
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|c| {
            let f = |i: usize| c.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32;
            [f(0), f(1), f(2), f(3)]
        })
        .collect();
    let height = w.get("height").and_then(Value::as_u64).unwrap_or(160) as u32;
    if !waveform || cols.is_empty() {
        return json_reply(info);
    }
    let (pw, ph, rgba) = effectcraft_engine::audio_analysis::waveform_rgba(&cols, height);
    let png = encode_png(pw, ph, rgba, 0)?;
    Ok(Reply::Image { png, info })
}

// ------------------------------------------------------------------ UI tools (bridge or offscreen UI)

fn screenshot(b: &mut Backend, a: &Value) -> Result<Reply> {
    let max_side = get(a, "max_side").and_then(Value::as_u64).unwrap_or(1600) as u32;
    let keep = get(a, "path").and_then(Value::as_str).map(str::to_string);
    if let (Some(c), Some(p)) = (b.confinement(), &keep) {
        c.check(p, crate::confine::Access::Write)?;
    }
    let path =
        keep.clone().unwrap_or_else(|| std::env::temp_dir().join(format!("effectcraft-mcp-shot-{}.png", std::process::id())).to_string_lossy().into_owned());
    // The app downscales before encoding (a full window is tens of MB otherwise).
    b.control("ui.screenshot", obj(&[("path", Some(&json!(path))), ("panel", get(a, "panel")), ("id", get(a, "id")), ("max_side", Some(&json!(max_side)))]))?;
    let bytes = std::fs::read(&path).map_err(|e| Error::Other(format!("screenshot: {e}")))?;
    if keep.is_none() {
        let _ = std::fs::remove_file(&path);
    }
    let (png, w, h) = recompress_png(&bytes, max_side)?;
    Ok(Reply::Image { png, info: json!({"width": w, "height": h, "path": keep}) })
}

fn passthrough(b: &mut Backend, method: &str, a: &Value) -> Result<Reply> {
    json_reply(b.control(method, a.clone())?)
}

fn ui_inspect(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.inspect", a)
}
fn render_times(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.renderTimes", a)
}
fn ui_elements(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.elements", a)
}
fn ui_click(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.click", a)
}
fn ui_drag(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.drag", a)
}
fn ui_key(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.key", a)
}
fn ui_type(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.type", a)
}
fn ui_set(b: &mut Backend, a: &Value) -> Result<Reply> {
    passthrough(b, "ui.set", a)
}
fn control(b: &mut Backend, a: &Value) -> Result<Reply> {
    let m = need(a, "method")?.as_str().ok_or_else(|| Error::BadArgs("`method` must be a string".into()))?;
    let p = params_of(get(a, "params"))?;
    json_reply(b.control(m, p)?)
}

// ------------------------------------------------------------------ the catalogue

static TOOLS: &[ToolDef] = &[
    crate::senses::EDIT_FEED,
    ToolDef {
        name: "list_commands",
        description: "List engine commands: id, label, menu path, shortcut, `params` doc (e.g. `{name?, width?, height?, frameRate?, duration? (s)}`) and whether it can run now (`why` if not). Every menu item, shortcut and panel gesture maps to one of these ids; run them with command_run. The full list is large (~130 KB): narrow with `filter` (e.g. `layer.new`, `keys`, `effect`) or pass `brief: true` for just ids and labels, then describe_command for one command's parameters.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "filter": {"type": "string", "description": "Only commands whose id or label contains this text (case-insensitive)."},
                    "enabled_only": {"type": "boolean", "description": "Only commands that can run right now."},
                    "schemas": {"type": "boolean", "description": "Also return each command's parameter JSON Schema (`schema`)."},
                    "brief": {"type": "boolean", "description": "Only {id, label} per command (~40 KB instead of ~125 KB); describe_command gives one command's params."}
                }),
                &[],
            )
        },
        run: list_commands,
    },
    ToolDef {
        name: "describe_command",
        description: "One command in full: id, label, menu path, shortcut, the `params` doc, a JSON Schema of its parameters (`schema`: properties, required, additionalProperties) and whether it can run now (`why` if not). Use before command_run when unsure of the parameters; command_run rejects unknown keys.",
        bridge_only: false,
        schema: || {
            schema(
                json!({"command": {"type": "string", "description": "Command id, e.g. `layer.newText`."}, "id": {"type": "string", "description": "Alias of `command`."}}),
                &[],
            )
        },
        run: describe_command,
    },
    ToolDef {
        name: "execute_command",
        description: "Run any engine command by id with JSON params; edits are undoable. Examples: comp.new {\"name\":\"Main\",\"width\":1920,\"height\":1080,\"frameRate\":30,\"duration\":5}; layer.newText {\"text\":\"Hello\",\"size\":120,\"fill\":\"#ffffff\"} (returns {\"layer\":id}); layer.newSolid {\"color\":\"#3366ff\"}; layer.newShape; effect.apply {\"layer\":3,\"effect\":\"Gaussian Blur\"}; time.set {\"time\":2.5}; layer.select {\"layers\":[3]}. Layers are referenced by id, \"#n\" or name; times are seconds.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "command": {"type": "string", "description": "Command id, e.g. `comp.new`, `layer.newText`, `effect.apply` (see command_list)."},
                    "id": {"type": "string", "description": "Alias of `command`."},
                    "params": {"type": "object", "description": "Command parameters as documented by command_list.", "additionalProperties": true}
                }),
                &[],
            )
        },
        run: execute_command,
    },
    ToolDef {
        name: "run_script",
        description: "Run JavaScript with an After Effects-style scripting object model (app, app.project, items.addComp, comp.layers.addSolid/addText/addShape/addNull/addCamera/addLight, layer.property(\"ADBE Transform Group\").property(\"ADBE Position\").setValueAtTime(t, v), Effects.addProperty(\"ADBE Gaussian Blur 2\"), expressions, markers, renderQueue…). Every edit is an undoable engine command; app.beginUndoGroup/endUndoGroup make one undo step. Returns {ok, result (last expression value), output (writeLn/$.writeln/alert text), error: {message, line, column} | null, rolledBack?, expressionErrors? (expressions the script broke: a missing marker, layer or match name)}. A script that fails or stops (after `timeout` seconds, default 300, 0 = no limit, or on MCP notifications/cancelled) undoes all its edits unless rollback: false. `file` runs a .jsx file (#include resolves next to it). File access is limited to the project folder unless the user allows scripts to write files.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "code": {"type": "string", "description": "JavaScript source, e.g. `var c = app.project.items.addComp(\"Main\", 1920, 1080, 1, 5, 30); c.layers.addText(\"Hi\"); c.numLayers`."},
                    "file": {"type": "string", "description": "Path of a .jsx file to run instead of `code` (absolute); `#include` resolves next to it. With `code`, only names the script so its includes resolve there."},
                    "name": {"type": "string", "description": "Script name used in error messages (default `script`, or `file`)."},
                    "timeout": {"type": "number", "minimum": 0, "description": "Stop the script after this many seconds (default 300; 0 = no limit)."},
                    "rollback": {"type": "boolean", "description": "Undo every edit of a script that fails (default true: no half-done state; the reply says rolledBack). false keeps the edits made before the error."},
                    "context": {"type": "boolean", "description": "Add observedContext (build, project, revision, client) to the reply (default false)."}
                }),
                &[],
            )
        },
        run: run_script,
    },
    ToolDef {
        name: "get_project",
        description: "Project overview: file path, dirty flag, bit depth, every item (id, name, type Composition/Footage/Solid/Folder, size, duration, layer count), the active comp id, current time (s), selected layers and the undo stack.",
        bridge_only: false,
        schema: || schema(json!({}), &[]),
        run: get_project,
    },
    ToolDef {
        name: "get_comp",
        description: "A composition: id, name, size, frame rate, duration, work area, background, `layerCount` and its layers top to bottom as compact rows {index, id, name, type, in, out} plus only what isn't the default: start (s, when ≠ 0), stretch (≠ 100), parent, trackMatte, blendMode (≠ Normal), switches (tags of the differing ones: hidden, 3d, solo, locked, shy, adjustment, motion_blur, guide, collapse, fx_off, audio_off, quality:…, sampling:…, frame_blend:…). Paged: up to `limit` layers (default 200) from `offset`; `next_offset` is null when every layer was returned, otherwise `truncated` says how to get the rest. `full: true` returns every field of every layer (label, all switches; long). Layer ids feed get_layer, get_property/set_property and commands.",
        bridge_only: false,
        schema: || {
            schema(
                json!({"comp": comp_s(), "full": full_s("layers"), "max_chars": max_chars_s(), "offset": offset_s(), "limit": limit_s("layers", crate::compact::COMP_LIMIT),
                    "summary": {"type": "boolean", "description": "true: the comp's settings, `layerCount`, layers per type (`types`), `hidden` and `parented` counts and only the top 20 layers."}}),
                &[],
            )
        },
        run: get_comp,
    },
    ToolDef {
        name: "get_layer",
        description: "A layer's property tree at the target comp time, with comp/revision/timeBase. value/rawValue are pre-expression; evaluatedValue is the expression result (expressionError reports fallback), and effectiveEnabled includes ancestor switches. Leaf properties have a `path` usable with get_property/set_property/add_keyframe, plus uid, match id, name, type, value at `time`, keyframes and expression. Groups contain children; inspect them here, then select a leaf path. `flat: true` returns just a list of {path, type, value, keys (count), expression} (compact), paged: up to `limit` (default 150) from `offset`, with `total`, `returned` and `next_offset` (null when done; `truncated` when cut). Replies fit `max_chars` (default 7000); oversized fields are explicit non-editable placeholders with a get_field pointer.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "layer": layer_s(), "comp": comp_s(),
                    "time": {"type": "number", "description": "Comp time in seconds for values (default: current time)."},
                    "depth": {"type": "integer", "description": "Group depth to expand (default all)."},
                    "flat": {"type": "boolean", "description": "Return a flat list of properties instead of the tree."},
                    "full": full_s("properties"), "max_chars": max_chars_s(), "offset": offset_s(), "limit": limit_s("properties (flat: true)", crate::compact::FLAT_LIMIT)
                }),
                &["layer"],
            )
        },
        run: get_layer,
    },
    ToolDef {
        name: "get_property",
        description: "Read one property at target comp time with comp/revision/timeBase: value/rawValue are pre-expression; evaluatedValue is the expression result (expressionError reports fallback). Includes uid, effectiveEnabled, whether it is animated, its expression and its keyframes, compact: {time (layer s), value (exact), compTime (only when it differs), in/out (only when not Linear)}. Keyframes are paged: `keyCount` total, up to `limit` (default 100) from `offset`, `next_offset` null when done (`truncated` when cut). `full: true` gives every key field (layerTime, compTime, in/out always).",
        bridge_only: false,
        schema: || {
            schema(
                json!({"layer": layer_s(), "path": path_s(), "comp": comp_s(), "time": {"type": "number", "description": "Comp time in seconds."}, "full": full_s("keyframes"), "max_chars": max_chars_s(), "offset": offset_s(), "limit": limit_s("keyframes", crate::compact::KEY_LIMIT)}),
                &["layer", "path"],
            )
        },
        run: get_property,
    },
    ToolDef {
        name: "get_result",
        description: "Read an immutable retained tool result without rerunning its action. Use result_id/result_mode/result_pointer from the reply; mode page returns array rows and mode json returns exact JSON text chunks. Handles expire on eviction or session end; never retry a mutation just to recover expired output.",
        bridge_only: false,
        schema: || {
            schema(
                json!({"result_id":{"type":"string"},"pointer":{"type":"string"},"mode":{"type":"string","enum":["page","json"]},"offset":offset_s(),"limit":limit_s("items or bytes",100),"max_chars":max_chars_s()}),
                &["result_id"],
            )
        },
        run: get_result,
    },
    ToolDef {
        name: "get_field",
        description: "Recover an oversized inspection field exactly. Repeat the original read arguments and pin comp/time to the original reply, set source to its tool name, copy the placeholder pointer, and page offset/limit through JSON text. Concatenate text chunks then parse JSON; placeholders are never editable values. offset/next_offset count UTF-8 bytes.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "source": {"type": "string", "enum": ["get_comp", "get_layer", "get_property", "get_project", "doc_inspect"]},
                    "comp": comp_s(), "layer": layer_s(), "path": path_s(), "time": {"type": "number"}, "flat": {"type": "boolean"}, "depth": {"type": "integer"},
                    "pointer": {"type": "string", "description": "JSON pointer from the omitted field."},
                    "offset": offset_s(), "limit": limit_s("JSON bytes", 4096), "max_chars": max_chars_s()
                }),
                &["source", "pointer"],
            )
        },
        run: get_field,
    },
    ToolDef {
        name: "set_property",
        description: "Set a property. Without `time`: sets the static value (or a key at the current time if already animated). With `time`: adds/replaces a keyframe there, turning the stopwatch on. `expression` sets an After Effects-style JS expression (\"\" removes it). Timed writes confirm requestedTime, actual snapped layerTime/compTime and appliedValue, read at that compTime. Returns the property as get_property does, plus `warnings` when the value was clamped to its valid range or reshaped (a number filling a vector, missing components set to 0). Popup properties take an option index (0-based) or name. Examples: {\"layer\":3,\"path\":\"transform/opacity\",\"value\":50}; {\"layer\":\"Title\",\"path\":\"transform/position\",\"value\":[200,540],\"time\":0}; {\"layer\":3,\"path\":\"transform/rotation\",\"expression\":\"time*90\"}.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "layer": layer_s(), "path": path_s(), "value": value_s(), "comp": comp_s(),
                    "time": {"type": "number", "description": "Keyframe time in seconds (layer time; equals comp time unless the layer is offset/stretched)."},
                    "timeBase": {"type": "string", "enum": ["layer", "comp"], "description": "What `time` counts: layer time (the default) or comp time, as the Timeline shows it."},
                    "expression": {"type": "string", "description": "Expression text; empty string removes the expression."}
                }),
                &["layer", "path"],
            )
        },
        run: set_property,
    },
    ToolDef {
        name: "add_keyframe",
        description: "Add keyframes to a property (turns the stopwatch on): one with `time` (+ optional `value`, default the current value) or many with `keys: [{time, value}]`, then optionally set their `interpolation`: linear, bezier, hold, easyEase, easyEaseIn, easyEaseOut. Example: {\"layer\":1,\"path\":\"transform/position\",\"keys\":[{\"time\":0,\"value\":[0,540]},{\"time\":2,\"value\":[1920,540]}],\"interpolation\":\"easyEase\"}.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "layer": layer_s(), "path": path_s(), "comp": comp_s(),
                    "time": {"type": "number", "description": "Key time in seconds (layer time)."},
                    "timeBase": {"type": "string", "enum": ["layer", "comp"], "description": "What `time` counts: layer time (the default) or comp time, as the Timeline shows it."},
                    "value": value_s(),
                    "keys": {"type": "array", "items": {"type": "object", "properties": {"time": {"type": "number"}, "value": value_s()}, "required": ["time"]}, "description": "Several keys at once."},
                    "interpolation": {"type": "string", "enum": ["linear", "bezier", "hold", "easyEase", "easyEaseIn", "easyEaseOut"]}
                }),
                &["layer", "path"],
            )
        },
        run: add_keyframe,
    },
    ToolDef {
        name: "add_effect",
        description: "Apply an effect to a layer and optionally set its parameters in one undoable call. `effect` is an id (`ec.blur.gaussian`) or After Effects name (`Gaussian Blur`; see list_effects); `values` maps parameter ids to values, e.g. {\"blurriness\": 12}. One undo step; if a value fails, nothing is applied. Returns the instance `path` (`effects/#n`, for set_property / add_keyframe) and its parameters with their paths and current values; values that were clamped to a parameter's range or otherwise adjusted are listed in `warnings`.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "layer": layer_s(), "comp": comp_s(),
                    "effect": {"type": "string", "description": "Effect id or name, e.g. `Gaussian Blur`, `Glow`, `Curves`, `ec.color.levels`."},
                    "values": {"type": "object", "description": "Parameter id -> value (ids are in the returned `params` paths), e.g. {\"blurriness\": 12}.", "additionalProperties": true}
                }),
                &["layer", "effect"],
            )
        },
        run: add_effect,
    },
    ToolDef {
        name: "list_effects",
        description: "The effect registry: id, After Effects name, category, GPU and 32-bpc support, and each parameter (id, name, default, valid `range` of sliders, popup `options`). The full list is large (~360 KB): narrow with `filter` (id, name or category text) or pass `brief: true` for just ids and names (~19 KB). Apply with add_effect.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "filter": {"type": "string", "description": "Only effects whose id, name or category contains this text."},
                    "brief": {"type": "boolean", "description": "Only {id, name} per effect (no parameters)."}
                }),
                &[],
            )
        },
        run: list_effects,
    },
    ToolDef {
        name: "list_fonts",
        description: "The font families text layers can use, bundled and installed on this machine: family, styles, origin (bundled / system / user), whether it is a variable font and the name in the font's own language. Narrow with `query` and `variable`; `rescan` picks up fonts installed since launch. Set one with command_run `layer.setText {\"layer\", \"font\", \"style\"}`.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "query": {"type": "string", "description": "Only families whose name (English or own-language) contains this text."},
                    "variable": {"type": "boolean", "description": "Only variable fonts (families with variation axes)."},
                    "rescan": {"type": "boolean", "description": "Look for fonts installed since launch first."}
                }),
                &[],
            )
        },
        run: list_fonts,
    },
    ToolDef {
        name: "get_state",
        description: "Where things stand: the editor state (active comp, current time, selected layers/keys/items, open comps, tool, work area) plus what this build can do (`app`: version, command and effect counts, GPU effects, import/export formats, After Effects parity summary). Cheap; call it to re-orient after a series of edits.",
        bridge_only: false,
        schema: || schema(json!({}), &[]),
        run: get_state,
    },
    ToolDef {
        name: "render_frame",
        description: "Render a composition frame (default: the active comp at the current time) and return it as a PNG image, longest side `max_side` (default 960, 0 = full size). `region` renders one rectangle at full resolution (detail checks); `layer` renders one layer solo; `item` renders a footage or solid item on its own; `overlays` draws layer bounds, anchor points, motion paths, safe margins or the grid on top and lists each layer's comp-space bounds. `path` also writes the PNG to disk; `inline: false` returns only the metadata. Use it to check your work; render_frames shows several times at once.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "item": item_s(),
                    "layer": solo_s(),
                    "time": {"type": "number", "description": "Comp time in seconds (default: current time); source time for `item`."},
                    "max_side": {"type": "integer", "description": "Longest side in pixels (default 960; 0 = full size of the frame or region)."},
                    "region": region_s(),
                    "overlays": overlays_s(),
                    "path": {"type": "string", "description": "Also write the PNG here."},
                    "inline": {"type": "boolean", "description": "Include the image in the reply (default true)."},
                    "transparent": {"type": "boolean", "description": "Keep the frame's alpha (RGBA PNG, as an RGB + Alpha render writes it) instead of compositing over the comp's background colour."}
                }),
                &[],
            )
        },
        run: render_frame,
    },
    ToolDef {
        name: "render_frames",
        description: "Several frames on ONE contact-sheet PNG, each cell labelled with its time and frame number: check an animation in one call. `times` (seconds) or `count` frames (default 6, at most 64) spread evenly over `start`..`end` (default the work area). `max_side` is the sheet's longest side (default 1600); `cols` overrides the grid. Takes render_preview's `layer`, `item`, `region`, `overlays` and `transparent`. `path` also writes the sheet, `gif` also writes an animated GIF of the frames. Returns the image plus {times, frames, cols, rows, cell, width, height}.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "item": item_s(),
                    "layer": solo_s(),
                    "times": {"type": "array", "items": {"type": "number"}, "description": "Comp times in seconds (at most 64)."},
                    "frames": {"type": ["array", "object"], "items": {"type": "integer"}, "description": "Comp frame numbers instead of `times`: a list ([190, 195, 200]) or every frame of a range ({\"from\": 190, \"to\": 210, \"every\": 1}, both ends included; at most 64)."},
                    "count": {"type": "integer", "minimum": 1, "description": "Frames spread evenly from `start` to `end` when `times` is not given (default 6, capped at 64)."},
                    "start": {"type": "number", "description": "First time in seconds (default: work area start)."},
                    "end": {"type": "number", "description": "End time in seconds, exclusive (default: work area end)."},
                    "cols": {"type": "integer", "minimum": 1, "description": "Columns (default: a grid close to 16:10)."},
                    "max_side": {"type": "integer", "description": "Longest side of the whole sheet in pixels (default 1600, 64-8192; 0 = 8192). Cells never exceed the frame's own size."},
                    "labels": {"type": "boolean", "description": "Label each cell with its time and frame number (default true)."},
                    "region": region_s(),
                    "overlays": overlays_s(),
                    "transparent": {"type": "boolean", "description": "Keep alpha (transparent gutters) instead of compositing over the comp background."},
                    "path": {"type": "string", "description": "Also write the sheet PNG here."},
                    "gif": {"type": "string", "description": "Also write an animated GIF of the frames here (delays follow the times)."},
                    "inline": {"type": "boolean", "description": "Include the image in the reply (default true)."}
                }),
                &[],
            )
        },
        run: render_frames,
    },
    ToolDef {
        name: "get_layout",
        description: "Where layers are on screen at a comp time (layer.bounds), without rendering: per layer whether it is drawn (`visible`; `why` not: in/out, Video switch, opacity 0, solo, guide...), its comp-space axis-aligned `bounds` [x,y,w,h] after transform, parenting and (3D) the active camera, the four `corners` (rotated layers), `center`, `anchorInComp`, the raw `position`/`anchor`, `onScreen` (0-1 share inside the comp frame) and the layer-space `source` rect (sourceRectAtTime). `pixels: true` also measures the drawn pixels (`pixelBounds`: masks, effects, styles); `overlaps: true` lists pairwise overlaps of the visible layers. Use it to check a title is centred, on screen or clear of other layers.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "time": {"type": "number", "description": "Comp time in seconds (default: current time)."},
                    "layers": {"type": "array", "items": layer_s(), "description": "Layers to measure (default every layer)."},
                    "pixels": {"type": "boolean", "description": "Also measure the drawn pixels incl. effects (renders each layer)."},
                    "overlaps": {"type": "boolean", "description": "Also return pairwise overlaps {layers, area, fraction} of the visible layers."}
                }),
                &[],
            )
        },
        run: get_layout,
    },
    ToolDef {
        name: "lint_frame",
        description: "Design lint (render.lint): plain-language design problems in a comp at one time (`time`, default the current time) or over `start`..`end` (sampled every `step` s plus in points and keyframes, at most 120 samples). Rules: text-too-small (on-screen font size under min_text_px, default comp height / 30), low-contrast (WCAG ratio of text against what is under it under min_contrast, default 3:1), outside-title-safe (text), outside-action-safe, cut-off (partly outside the frame), text-overlap, hidden-visible (switched on but off-screen, transparent or covered), off-beat (keyframes and in points just missing the audio's beat), off-accent (speed-graph peaks just missing a stressed syllable of the comp's speech map, see speech.import / speech.accents), nested-cut-off (layers inside precomps measured in this comp's frame), text-off-centre (text vs its card/pill, measured at the text's rest pose; `fix` is the prop.set that centres it), exposed-edge (empty frame edges, transition gaps), exit-at-cut (a full-screen cutaway fading out into the comp's end or gone before it: hold to the end and hard-cut), off-taste (eases off Elias's profile, see taste_check). off-accent on keys you made is an error. Returns {clean, counts, problems: [{rule, severity, layer, layerName, time, until, message, other?, inner?, fix? {command, params}}]}; each message says what to fix. Run it before calling a design done.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "time": {"type": "number", "description": "One comp time in seconds (default: current time). Use start/end for a range instead."},
                    "start": {"type": "number", "description": "Range start, comp seconds (default 0 when end is given)."},
                    "end": {"type": "number", "description": "Range end, comp seconds (default the comp's duration when start is given)."},
                    "step": {"type": "number", "description": "Seconds between range samples (default 0.5)."},
                    "rules": {"type": "array", "items": {"type": "string", "enum": effectcraft_engine::lint::RULES}, "description": "Only these rules (default all)."},
                    "min_text_px": {"type": "number", "description": "Smallest legible font size on screen, comp pixels (default comp height / 30)."},
                    "min_contrast": {"type": "number", "description": "Smallest acceptable WCAG contrast ratio (default 3)."},
                    "summary": {"type": "boolean", "description": "true: counts per rule (`byRule`) and per layer (`byLayer`, top 10) plus the 10 worst problems instead of every problem. Default: on when there are more than 40 problems; false lists them all."}
                }),
                &[],
            )
        },
        run: lint_frame,
    },
    ToolDef {
        name: "taste_check",
        description: "Elias's ease taste check (taste.check): every keyframed move (position, scale, rotation, opacity; `all` for other properties) judged against his ease profile. ✅ approved = one of his approved curves for that move type (entrance, exit, on-screen move, fade-in, fade-out, scale pop); 🟡 close = inside his learned ranges but not an approved curve (the message says how it differs from the nearest, e.g. \"like Expo Out 1, launch softer (out 12 % vs 0.1 %)\"); ❌ off = linear, Easy Ease 33/33, a speed jump at a key, baked one-key-per-frame, or outside his ranges (speed peak position for the move type, peak sharpness for its length, speed cap in % of the frame per frame, frames on the last 10 % of an entrance). A ❌ is `blocking` once its move type has enough approved curves (else warn only). Each non-✅ move carries `fix`: the taste.apply call (command_run) that puts the nearest approved curve on it. Returns {summary {line}, clean, moves: [...]}. Run it over the animated range before handing a comp over; new curves go to taste.propose (⏳ until Elias approves).",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "layer": layer_s(),
                    "layers": {"type": "array", "items": layer_s(), "description": "Only these layers (default every layer)."},
                    "start": {"type": "number", "description": "Range start, comp seconds (default the comp's start)."},
                    "end": {"type": "number", "description": "Range end, comp seconds (default the comp's end)."},
                    "all": {"type": "boolean", "description": "Also other animated properties (hard fails only)."},
                    "problems_only": {"type": "boolean", "description": "Leave out the ✅ moves."}
                }),
                &[],
            )
        },
        run: taste_check,
    },
    ToolDef {
        name: "ease_graph",
        description: "A property's value graph with its speed graph under it, as an image (taste.graph): keys marked with their frames, each move's speed curve coloured by its taste verdict (green ✅ approved, amber 🟡 close, red ❌ off) and labelled with its curve. Use it to see an ease without rendering, and to show Elias a move. `path` keeps a copy of the PNG.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "layer": layer_s(),
                    "property": {"type": "string", "description": "Property path, e.g. transform/position."},
                    "prop": {"type": "integer", "description": "Property uid instead of `property`."},
                    "start": {"type": "number", "description": "Range start, comp seconds (default the keys with a margin)."},
                    "end": {"type": "number", "description": "Range end, comp seconds."},
                    "width": {"type": "integer", "description": "Image width in px (default 1280)."},
                    "height": {"type": "integer", "description": "Image height in px (default 720)."},
                    "path": {"type": "string", "description": "Also save the PNG here."}
                }),
                &["layer"],
            )
        },
        run: ease_graph,
    },
    ToolDef {
        name: "get_motion",
        description: "How layers move over a time range, in words and numbers, without rendering (comp.motion / layer.motion): samples each layer's comp-space content box, opacity, scale and rotation (default the whole comp, every frame or coarser so a layer gets at most 600 samples; keyframe times are always sampled) and returns per layer `segments` {kind: enter|exit|move|hold|scale|rotate|fade-in|fade-out, start, end, from, to, direction (move/enter/exit: left, up-right...), edge (enter/exit: the frame edge)}, `offscreen` intervals and a one-sentence `summary` (\"Logo flies in from the left 0–1s, holds 1–4s, fades out 4–5s.\"), plus `overlaps` {a, b, start, end, maxFraction} between drawn layers (full-frame backgrounds left out). Parenting: a layer whose moves come from animated parents lists them in `movedBy` and its summary names the layer to key; a null (or other no-picture layer) that parents layers and is keyed is described by what it moves (`moves`, `describedBy`: \"S1 RIG is a null that moves its 3 layers (…): it flies in from the right 21.65–22.26s\"). Times are comp seconds. Use it to check an animation's timing and choreography; render_frames shows the frames.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "layer": layer_s(),
                    "layers": {"type": "array", "items": layer_s(), "description": "Layers to describe (default every layer of the comp); `layer` describes one."},
                    "start": {"type": "number", "description": "Start, comp seconds (default 0)."},
                    "end": {"type": "number", "description": "End, comp seconds (default the comp's duration)."},
                    "step": {"type": "number", "description": "Sampling step in seconds (default one frame, coarser for comps over ~10 s so a layer gets at most 600 samples)."}
                }),
                &[],
            )
        },
        run: get_motion,
    },
    ToolDef {
        name: "analyze_audio",
        description: "Measure the comp's audio mix (what preview and export play) over [start, end) seconds (audio.analyze): peakDbfs, truePeakDbtp (4x oversampled), rmsDbfs, integratedLufs (ITU-R BS.1770, gated), momentary / short-term max LUFS, clippedSamples, silent ranges (below silence_db for at least min_silence s) and per-channel stats; null levels mean silence. `layer` measures one layer alone. `waveform: true` returns a waveform image (left channel on top, red where it clips). A comp without audio returns hasAudio: false.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "start": {"type": "number", "description": "Start, comp seconds (default 0)."},
                    "end": {"type": "number", "description": "End, comp seconds (default the comp's duration)."},
                    "layer": layer_s(),
                    "silence_db": {"type": "number", "description": "Silence threshold in dBFS (default -60)."},
                    "min_silence": {"type": "number", "description": "Shortest silence reported, seconds (default 0.25)."},
                    "true_peak": {"type": "boolean", "description": "Estimate the true peak (default true)."},
                    "waveform": {"type": "boolean", "description": "Return a waveform image."},
                    "width": {"type": "integer", "description": "Waveform width in pixels (default 800)."},
                    "height": {"type": "integer", "description": "Waveform height in pixels (default 160)."},
                    "path": {"type": "string", "description": "Also write the waveform PNG here."}
                }),
                &[],
            )
        },
        run: analyze_audio,
    },
    ToolDef {
        name: "render_snapshot",
        description: "Render a frame (default: the active comp at the current time) and keep it in the session under `name`, for render_compare. Verify an edit by looking: render_snapshot {\"name\":\"before\"} -> edit -> render_compare {\"a\":\"before\",\"b\":{}}. Up to 16 are kept (oldest dropped); taking a name again replaces it.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "name": {"type": "string", "description": "Snapshot name."},
                    "comp": comp_s(),
                    "time": {"type": "number", "description": "Comp time in seconds (default: current time)."},
                    "max_side": {"type": "integer", "minimum": 0, "description": "Longest side in pixels (default 0 = full comp size)."},
                    "transparent": {"type": "boolean", "description": "Keep the frame's alpha instead of compositing over the comp background."}
                }),
                &["name"],
            )
        },
        run: render_snapshot,
    },
    ToolDef {
        name: "render_compare",
        description: "Compare two frames: snapshots (render_snapshot), image files (goldens, After Effects reference renders) or frames rendered now. Returns {width, height, max_abs, mean_abs (8-bit levels), alpha_max_abs, psnr_db (\"inf\" when identical), ssim, changed_pixels, changed_pct, pass, diffBounds ([x, y, w, h] box of the changed pixels, null when none)} and, when something changed, the diff heatmap as an image (red -> yellow -> white by size of difference, over frame a dimmed to grey). `pass` = no channel differs by more than `threshold` levels (default 1), or with `min_psnr` PSNR >= min_psnr. Sizes: `max_side` scales both down first; the same aspect ratio at different sizes compares at the smaller size; different aspect ratios are an error. `region` compares just one rectangle.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "a": frame_source_s(),
                    "b": frame_source_s(),
                    "threshold": {"type": "number", "minimum": 0, "maximum": 255, "description": "Per-channel tolerance in 8-bit levels for `changed_pixels` and `pass` (default 1)."},
                    "min_psnr": {"type": "number", "minimum": 0, "description": "Pass when PSNR (dB) is at least this, instead of the exact threshold test (e.g. 35 for another app's render)."},
                    "max_side": {"type": "integer", "minimum": 0, "description": "Scale both down to this longest side before comparing (0 = full size, the default)."},
                    "region": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4, "description": "[x, y, w, h] in pixels of the compared frames (comp pixels unless scaled): metrics, diffBounds and the heatmap cover only this rectangle (clipped to the frame)."},
                    "amplify": {"type": "number", "minimum": 1, "maximum": 255, "description": "Heatmap gain (default 8: a 32-level difference is white)."},
                    "diff_path": {"type": "string", "description": "Also write the heatmap PNG here (full size)."},
                    "inline": {"type": "boolean", "description": "Include the heatmap image in the reply when something changed (default true)."}
                }),
                &["a", "b"],
            )
        },
        run: render_compare,
    },
    ToolDef {
        name: "hosts",
        description: "Where projects are open: every running EffectCraft (the desktop app, headless servers) with its control port and project. `join` (a port, or the path of a project one of them has open) switches this server onto that host, as your own `client`, so you work in the same project as the user and other agents instead of on a copy (claim your comp with command_run comp.claim). Refused while this server's own project has unsaved changes.",
        bridge_only: false,
        schema: || {
            schema(json!({"join": {"type": ["integer", "string"], "description": "A host's control port, or the path of a project a host has open."}}), &[])
        },
        run: hosts,
    },
    ToolDef {
        name: "open_project",
        description: "Open a project: `path` to an .ecproj file, `demo: true` for the built-in demo project, or `new: true` for an empty project. Returns the project overview. Refused while this host's current project has unsaved changes, unless `discard: true`.",
        bridge_only: false,
        schema: || {
            schema(
                json!({"path": {"type": "string", "description": "Path of an .ecproj file."}, "demo": {"type": "boolean"}, "new": {"type": "boolean"}, "discard": {"type": "boolean", "description": "Drop the current unsaved project instead of refusing."}}),
                &[],
            )
        },
        run: open_project,
    },
    ToolDef {
        name: "save_project",
        description: "Save the project to its current file, or to `path` (Save As, .ecproj).",
        bridge_only: false,
        schema: || schema(json!({"path": {"type": "string", "description": "Destination .ecproj path (Save As)."}}), &[]),
        run: save_project,
    },
    ToolDef {
        name: "undo",
        description: "Undo the last `steps` edits (default 1). Returns the remaining undo stack.",
        bridge_only: false,
        schema: || schema(json!({"steps": {"type": "integer", "minimum": 1}}), &[]),
        run: undo,
    },
    ToolDef {
        name: "redo",
        description: "Redo `steps` undone edits (default 1).",
        bridge_only: false,
        schema: || schema(json!({"steps": {"type": "integer", "minimum": 1}}), &[]),
        run: redo,
    },
    ToolDef {
        name: "history",
        description: "The branching undo history (History panel): without arguments lists every state {index, id, label, parent, depth (0 = working line, >0 = branch), current, future}; undoing and then editing keeps the undone states as a branch. With `goto` (an index or id from the listing) jumps to that state, on any branch, without losing the others.",
        bridge_only: false,
        schema: || schema(json!({"goto": {"type": ["integer", "string"], "description": "State index or id to jump to (edit.history.goto)."}}), &[]),
        run: history_tool,
    },
    ToolDef {
        name: "script_ui",
        description: "Drive ScriptUI windows that scripts opened (dialogs, palettes, dockable panels). action `list` (default): open windows; `get`: a window's control tree (type, name, text, value, checked, items, selection, bounds); `click`: press a button / toggle a checkbox / pick a radio button; `set`: type text, move a slider, pick a list item (index or text); `close`: close a window (`result` for a dialog's show(), default 2 = Cancel). Controls by id, \"#id\", properties.name or text. A dialog's script resumes when the dialog closes.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "action": {"type": "string", "enum": ["list", "get", "click", "set", "close"]},
                    "window": {"type": ["integer", "string"], "description": "Window id or title (optional when one is open)."},
                    "widget": {"type": ["integer", "string"], "description": "Control id, \"#id\", properties.name or text."},
                    "value": {"description": "For `set`: text, number, bool, item index or item text."},
                    "result": {"type": "integer", "description": "For `close`: what the dialog's show() returns."}
                }),
                &[],
            )
        },
        run: script_ui_tool,
    },
    ToolDef {
        name: "screenshot",
        description: "Screenshot of the app window (the live one when bridged, the offscreen one with `mcp --ui`), or one panel (`Composition`, `Timeline`, `EffectControls`, `Project`, ...) or element id, as PNG (longest side `max_side`, default 1600). Use it to check what the UI looks like after an action.",
        bridge_only: true,
        schema: || {
            schema(
                json!({
                    "panel": {"type": "string", "description": "Panel to crop to."},
                    "id": {"type": "string", "description": "Element id (from ui_elements) to crop to."},
                    "max_side": {"type": "integer"},
                    "path": {"type": "string", "description": "Keep the full-size PNG at this path."}
                }),
                &[],
            )
        },
        run: screenshot,
    },
    ToolDef {
        name: "ui_inspect",
        description: "UI state: window size, tool, workspace, panels, viewer and timeline view state, playback, active comp, selection, open dialog, render time.",
        bridge_only: true,
        schema: || schema(json!({}), &[]),
        run: ui_inspect,
    },
    ToolDef {
        name: "render_times",
        description: "Why was this frame slow? The render times of the frame the viewer shows (the Timeline's Render Time pane): frame {ms, backend: gpu|cpu, reason, frameDurationMs, slow, cpuLayers} and each layer, slowest first, {comp, layer, name, ms, backend: gpu|cpu, reason} (e.g. \"Taper or Wave stroke: the GPU can't draw it\", \"effect X has no GPU version\", \"the GPU ran out of memory\").",
        bridge_only: true,
        schema: || schema(json!({}), &[]),
        run: render_times,
    },
    ToolDef {
        name: "ui_elements",
        description: "Interactive elements drawn in the last frame: id, label, rect [x,y,w,h] in points. Stable ids such as `tools.Selection`, `panel.Timeline`, `panel.tab.EffectControls`, `project.item.<id>`, `viewer.comp`, `timeline.layer.<id>.bar`, `timeline.prop.<uid>.stopwatch`, `timeline.cti`, `effects.item.<name>`. Filter with `prefix`.",
        bridge_only: true,
        schema: || schema(json!({"prefix": {"type": "string", "description": "Only ids starting with this, e.g. `tools.`, `timeline.layer.`."}}), &[]),
        run: ui_elements,
    },
    ToolDef {
        name: "ui_click",
        description: "Click an element by `id` (from ui_elements) or at `x`,`y` (points). `button`: left|right|middle, `count`: 2 for double-click, `modifiers`: {shift, alt, command, ctrl}.",
        bridge_only: true,
        schema: || {
            schema(
                json!({
                    "id": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"},
                    "button": {"type": "string", "enum": ["left", "right", "middle"]},
                    "count": {"type": "integer"}, "modifiers": {"type": "object"}
                }),
                &[],
            )
        },
        run: ui_click,
    },
    ToolDef {
        name: "ui_drag",
        description: "Press-move-release from `from` to `to`; each is {\"id\": element} (add fx/fy 0-1 for a spot inside it) or {\"x\",\"y\"}. Moves layers in the viewer, drags layer bars/keyframes/the CTI (`timeline.cti`), scrubs values, draws shapes with a shape tool.",
        bridge_only: true,
        schema: || {
            schema(
                json!({"from": {"type": "object"}, "to": {"type": "object"}, "steps": {"type": "integer"}, "modifiers": {"type": "object"}}),
                &["from", "to"],
            )
        },
        run: ui_drag,
    },
    ToolDef {
        name: "ui_key",
        description: "Press a key or shortcut: `Space` (play), `F9` (Easy Ease), `Cmd+Z`, `Cmd+N`, `PageDown`, `Escape`, `Enter` (`Cmd` = Ctrl off macOS).",
        bridge_only: true,
        schema: || schema(json!({"key": {"type": "string"}}), &["key"]),
        run: ui_key,
    },
    ToolDef {
        name: "ui_type",
        description: "Type text into the focused field (text layer editing, search boxes, value fields).",
        bridge_only: true,
        schema: || schema(json!({"text": {"type": "string"}}), &["text"]),
        run: ui_type,
    },
    ToolDef {
        name: "ui_set",
        description: "Set UI state directly: tool, workspace, theme (dark|darker|light), focused panel, viewer {zoom, res, pan, grid, rulers, safeMargins, transparencyGrid}, timeline {pps, start, graphEditor, showModes, openLayers, openGroups}, menuBar, home.",
        bridge_only: true,
        schema: || {
            schema(
                json!({
                    "tool": {"type": "string"}, "workspace": {"type": "string"}, "theme": {"type": "string"}, "focused": {"type": "string"},
                    "viewer": {"type": "object"}, "timeline": {"type": "object"}, "menuBar": {"type": "boolean"}, "home": {"type": "boolean"}
                }),
                &[],
            )
        },
        run: ui_set,
    },
    ToolDef {
        name: "control",
        description: "Call any control-channel method (docs/control-protocol.md) with its params, e.g. ui.menu.list, ui.menu.invoke {id}, ui.panel.show {panel}, ui.scroll, ui.viewer.locate {x,y}, ui.timeline.locate {layer,time}, ui.playback {action}, ui.renderTimes, ui.resize.",
        bridge_only: true,
        schema: || schema(json!({"method": {"type": "string"}, "params": {"type": "object", "additionalProperties": true}}), &["method"]),
        run: control,
    },
    ToolDef {
        name: "batch",
        description: "Run several engine commands in one call as ONE undo step (engine.batch). Later steps can use earlier results: a string param \"$N\" or \"$N.key\" is step N's result (1-based), e.g. create a layer, then key and style it: {\"steps\":[{\"command\":\"layer.newText\",\"params\":{\"text\":\"Hi\",\"name\":\"Title\"}},{\"command\":\"prop.addKey\",\"params\":{\"layer\":\"$1.layer\",\"path\":\"transform/opacity\",\"time\":0,\"value\":0}},{\"command\":\"effect.apply\",\"params\":{\"layer\":\"$1.layer\",\"effect\":\"Glow\"}}]}. Returns {steps, results}. If a step fails the whole batch is rolled back (unless atomic: false) and the error names the step.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "steps": {"type": "array", "items": {"type": "object", "properties": {"command": {"type": "string"}, "params": {"type": "object", "additionalProperties": true}}, "required": ["command"]}},
                    "label": {"type": "string", "description": "Undo step name (default Batch)."},
                    "atomic": {"type": "boolean", "description": "Roll everything back when a step fails (default true)."}
                }),
                &["steps"],
            )
        },
        run: batch_tool,
    },
    ToolDef {
        name: "command_list",
        description: "List engine commands: id, label, menu path, shortcut, `params` doc (e.g. `{name?, width?, height?, frameRate?, duration? (s)}`) and whether it can run now (`why` if not). Every menu item, shortcut and panel gesture maps to one of these ids; run them with command_run. The full list is large (~130 KB): narrow with `filter` (e.g. `layer.new`, `keys`, `effect`) or pass `brief: true` for just ids and labels, then describe_command for one command's parameters.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "filter": {"type": "string", "description": "Only commands whose id or label contains this text (case-insensitive)."},
                    "enabled_only": {"type": "boolean", "description": "Only commands that can run right now."},
                    "schemas": {"type": "boolean", "description": "Also return each command's parameter JSON Schema (`schema`)."},
                    "brief": {"type": "boolean", "description": "Only {id, label} per command (~40 KB instead of ~125 KB); describe_command gives one command's params."}
                }),
                &[],
            )
        },
        run: list_commands,
    },
    ToolDef {
        name: "command_run",
        description: "Run any engine command by id with JSON params; edits are undoable. Examples: comp.new {\"name\":\"Main\",\"width\":1920,\"height\":1080,\"frameRate\":30,\"duration\":5}; layer.newText {\"text\":\"Hello\",\"size\":120,\"fill\":\"#ffffff\"} (returns {\"layer\":id}); layer.newSolid {\"color\":\"#3366ff\"}; layer.newShape; effect.apply {\"layer\":3,\"effect\":\"Gaussian Blur\"}; time.set {\"time\":2.5}; layer.select {\"layers\":[3]}. Layers are referenced by id, \"#n\" or name; times are seconds.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "id": {"type": "string", "description": "Command id, e.g. `comp.new`, `layer.newText`, `effect.apply` (see command_list)."},
                    "params": {"type": "object", "description": "Command parameters as documented by command_list.", "additionalProperties": true}
                }),
                &["id"],
            )
        },
        run: execute_command,
    },
    ToolDef {
        name: "render_preview",
        description: "Render a composition frame (default: the active comp at the current time, in comp seconds) and return it as a PNG image, longest side `max_side` (default 960, 0 = full size). `region` renders one rectangle at full resolution (detail checks); `layer` renders one layer solo; `item` renders a footage or solid item on its own; `overlays` draws layer bounds, anchor points, motion paths, safe margins or the grid on top and lists each layer's comp-space bounds. `path` also writes the PNG to disk; `inline: false` returns only the metadata. Use it to check your work; render_frames shows several times at once.",
        bridge_only: false,
        schema: || {
            schema(
                json!({
                    "comp": comp_s(),
                    "item": item_s(),
                    "layer": solo_s(),
                    "time": {"type": "number", "description": "Comp time in seconds (default: current time); source time for `item`."},
                    "max_side": {"type": "integer", "description": "Longest side in pixels (default 960; 0 = full size of the frame or region)."},
                    "region": region_s(),
                    "overlays": overlays_s(),
                    "path": {"type": "string", "description": "Also write the PNG here."},
                    "inline": {"type": "boolean", "description": "Include the image in the reply (default true)."},
                    "transparent": {"type": "boolean", "description": "Keep the frame's alpha (RGBA PNG, as an RGB + Alpha render writes it) instead of compositing over the comp's background colour."}
                }),
                &[],
            )
        },
        run: render_frame,
    },
    ToolDef {
        name: "command_batch",
        description: "Run commands in order, returning {completed, failed, results, failedSteps?}: each step's result (big ones shortened to their ids unless verbose) or error. Stop at the first error by default; stop_on_error: false continues. Each edit has its own undo step; use batch for an atomic undo group.",
        bridge_only: false,
        schema: || {
            schema(
                json!({"steps": {"type": "array", "items": {"type": "object", "properties": {"id": {"type": "string"}, "params": {"type": "object"}}, "required": ["id"], "additionalProperties": false}}, "stop_on_error": {"type": "boolean"}, "verbose": {"type": "boolean", "description": "Inline every step's full result (default: results over 300 characters keep only their ids, e.g. layer, comp, item)."}}),
                &["steps"],
            )
        },
        run: command_batch,
    },
    ToolDef {
        name: "doc_inspect",
        description: "Inspect the project overview and active composition (null when no composition is active), its layers as compact, paged rows like get_comp (`full`, `offset`, `limit` work the same).",
        bridge_only: false,
        schema: || {
            schema(
                json!({"full": full_s("active comp layers"), "max_chars": max_chars_s(), "offset": offset_s(), "limit": limit_s("layers", crate::compact::COMP_LIMIT)}),
                &[],
            )
        },
        run: doc_inspect,
    },
];

fn doc_inspect(b: &mut Backend, a: &Value) -> Result<Reply> {
    let mut project = b.exec("project.summary", json!({}))?;
    let revision = project["revision"].clone();
    let comp_id = project.get("activeComp").cloned().ok_or_else(|| Error::App("project.summary omitted activeComp; inspection is incomplete".into()))?;
    if !comp_id.is_null() && !comp_id.is_u64() {
        return Err(Error::App("project.summary returned an invalid activeComp; inspection is incomplete".into()));
    }
    let unbounded = get(a, "full").and_then(Value::as_bool) == Some(true) && !crate::compact::Page::requested(a);
    let mut comp_args = a.clone();
    let budget = crate::compact::budget(a)?;
    if !unbounded {
        if budget < 3072 {
            return Err(Error::BadArgs("doc_inspect needs max_chars >= 3072; use get_comp/get_project for smaller replies".into()));
        }
        let project_budget = budget / 3;
        project = crate::compact::bounded_reply(project, &json!({"max_chars": project_budget}))?;
        prefix_field_pointers(&mut project, "/project", 0);
        comp_args["max_chars"] = json!(budget.saturating_sub(project_budget).saturating_sub(512));
    }
    let mut comp = if comp_id.is_null() { Value::Null } else { crate::compact::comp_reply(b.exec("comp.info", json!({"comp": comp_id}))?, &comp_args)? };
    prefix_field_pointers(&mut comp, "/activeComp", 0);
    let out = json!({"revision": revision, "project": project, "activeComp": comp});
    // Never replace a selected page after recording its returned/next_offset metadata.
    if !unbounded && out.to_string().len() > budget {
        return Err(Error::BadArgs("doc_inspect metadata exceeds max_chars; increase it or inspect get_project/get_comp separately".into()));
    }
    json_reply(out)
}

fn prefix_field_pointers(value: &mut Value, prefix: &str, depth: usize) {
    if depth >= 64 {
        return;
    }
    match value {
        Value::Object(o) => {
            if o.get("$omitted").and_then(Value::as_bool) == Some(true) {
                if let Some(pointer) = o.get("pointer").and_then(Value::as_str) {
                    o.insert("pointer".into(), json!(format!("{prefix}{pointer}")));
                }
            } else {
                for child in o.values_mut() {
                    prefix_field_pointers(child, prefix, depth + 1);
                }
            }
        }
        Value::Array(a) => {
            for child in a {
                prefix_field_pointers(child, prefix, depth + 1);
            }
        }
        _ => {}
    }
}

/// The longest step result `command_batch` inlines without `verbose`.
const STEP_INLINE_CHARS: usize = 300;

fn command_batch(b: &mut Backend, a: &Value) -> Result<Reply> {
    let input = need(a, "steps")?.as_array().ok_or_else(|| Error::BadArgs("steps must be an array".into()))?;
    if input.len() > 1024 {
        return Err(Error::BadArgs("at most 1024 commands per call".into()));
    }
    let mut steps = Vec::with_capacity(input.len());
    for step in input {
        let command =
            get(step, "command").or_else(|| get(step, "id")).and_then(Value::as_str).ok_or_else(|| Error::BadArgs("each step needs id or command".into()))?;
        steps.push(json!({"command":command,"params":params_of(get(step,"params"))?}));
    }
    let done = b.exec(
        "engine.batch",
        json!({"steps":steps,"atomic":false,"stopOnError":get(a,"stop_on_error").and_then(Value::as_bool).unwrap_or(true),"groupUndo":false}),
    )?;
    // Short by default: a step's result is inlined when small; a big one keeps only its ids
    // (`layer`, `comp`, `item`…) and failed steps are also listed together (`verbose`: all).
    let verbose = get(a, "verbose").and_then(Value::as_bool) == Some(true);
    let ids = input.iter().map(|st| get(st, "command").or_else(|| get(st, "id")).cloned().unwrap_or(Value::Null));
    let mut failed_steps = vec![];
    let results: Vec<Value> = done["executionResults"]
        .as_array()
        .into_iter()
        .flatten()
        .zip(ids)
        .enumerate()
        .map(|(i, (r, id))| {
            if r["ok"] != true {
                failed_steps.push(json!({"step": i + 1, "id": id, "error": r["error"]}));
                return r.clone();
            }
            match r.get("result") {
                Some(v) if !verbose && v.to_string().len() > STEP_INLINE_CHARS => {
                    let mut short = json!({"ok": true, "omitted": true});
                    for key in
                        ["id", "layer", "layers", "comp", "item", "items", "effect", "key", "keys", "path", "name", "index", "time", "warning", "warnings"]
                    {
                        if let Some(x) = v.get(key).filter(|x| x.to_string().len() <= STEP_INLINE_CHARS) {
                            short[key] = x.clone();
                        }
                    }
                    short
                }
                _ => r.clone(),
            }
        })
        .collect();
    let mut out = json!({"completed":done["completed"],"failed":done["failed"],"results":results});
    if !failed_steps.is_empty() {
        out["failedSteps"] = json!(failed_steps);
    }
    for key in ["appliedSteps", "files", "fileCoverage", "untrackedEffectsPossible", "rolledBack", "failedStep"] {
        if let Some(v) = done.get(key) {
            out[key] = v.clone();
        }
    }
    json_reply(out)
}
