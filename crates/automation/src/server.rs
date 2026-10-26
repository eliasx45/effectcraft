//! MCP server: JSON-RPC 2.0, one message per line (the MCP stdio transport). Implements
//! tools, resources (with templates and subscriptions), prompts and logging. The stdio loop
//! handles progress and cancellation for long renders and writes queued notifications.
//!
//! A `tools/call` carrying `_meta.requestId` (or a `request_id` argument) is answered from a small
//! cache when the same id comes again, so a retried call never runs twice.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{Value, json};

use crate::autosave::AutoSave;
use crate::notify::Notifier;
use crate::prompts;
use crate::resources::{self, UriError};
use crate::tools::{self, Reply};
use crate::{Backend, Error, base64};

/// Protocol revisions we speak, newest first. We answer with the client's if we know it.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "EffectCraft is an After Effects-class motion graphics compositor. Everything is an engine command: command_list discovers ids and params (describe_command shows one in full), command_run {id, params} runs them (undoable; command_batch runs several in order, batch as one undo step). Typical flow: command_run comp.new -> command_run layer.newText / layer.newSolid (returns the layer id) -> set_property / add_keyframe / add_effect -> render_preview to look at the result -> lint_frame to catch design problems (text too small or low-contrast, outside title safe, cut off, overlapping, hidden, off the beat). Inspect with doc_inspect, get_comp and get_layer (every property node carries its `path`, e.g. `transform/position`, `effects/#1/blurriness`). Times are seconds: use timeBase:\"comp\" for add_keyframe and set_property with `time` so keys use the Timeline clock; the default is layer time. Writes snap to comp frames; use returned compTime/layerTime for the applied time. render_preview and get_property use comp time. Resources and resource templates (effectcraft://document, comp/{comp}, layer/{comp}/{layer}, frame/{comp}/{time}, command/{id}) and workflow prompts are available too. With a UI (bridge mode `--bridge <port>` to a running app, or `--ui` for the app offscreen) screenshot and the ui_* tools show and operate it. Several agents at once: pass your name as `client` on any tool call (or start the server with --client NAME) to get your own selection, active comp and time; command_run comp.claim reserves a comp against other agents' edits.";

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const RESOURCE_NOT_FOUND: i64 = -32002;

/// The first revision with `structuredContent`, `outputSchema` and `title`.
const STRUCTURED_SINCE: &str = "2025-06-18";
/// Tool-call `requestId`s remembered (with their results) for retries.
const REPLY_CACHE: usize = 128;

pub struct McpServer {
    backend: Backend,
    autosave: Option<AutoSave>,
    /// The protocol revision `initialize` negotiated (`None` before it).
    version: Option<&'static str>,
    notify: Notifier,
    /// List the hidden aliases too (`mcp --all-tools`).
    all_tools: bool,
    /// Recent `tools/call` results by request id.
    replies: VecDeque<(String, Value)>,
}

impl McpServer {
    pub fn new(mut backend: Backend) -> Self {
        let notify = Notifier::new(&mut backend);
        Self { backend, autosave: None, version: None, notify, all_tools: false, replies: VecDeque::new() }
    }

    /// Also list the older tool names (`list_commands`, `execute_command`, `get_project`,
    /// `render_frame`) in `tools/list`. They are callable either way.
    pub fn with_all_tools(mut self, all: bool) -> Self {
        self.all_tools = all;
        self
    }

    /// Server-initiated notifications queued since the last call (resource updates, log
    /// entries), oldest first. [`McpServer::serve`] writes them after each reply.
    pub fn take_notifications(&mut self) -> Vec<Value> {
        self.notify.take()
    }

    /// Write the queued notifications, one per line.
    pub(crate) fn flush_notifications(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let queued = self.notify.take();
        for n in &queued {
            out.write_all(n.to_string().as_bytes())?;
            out.write_all(b"\n")?;
        }
        if !queued.is_empty() {
            out.flush()?;
        }
        Ok(())
    }

    /// Whether replies to this request may use 2025-06-18 fields (`structuredContent`,
    /// `outputSchema`, `title`): the session negotiated it, or the request declares a modern revision.
    pub(crate) fn structured(&self, params: &Value) -> bool {
        is_modern(params) || self.version.is_some_and(|v| v >= STRUCTURED_SINCE)
    }

    /// Opt in to durable checkpoints in a separate folder per server, for a session in this
    /// process: headless, or the offscreen app of `mcp --ui` (checkpointed on its UI thread).
    /// `root` is the platform config directory; settings are read by the caller.
    pub fn with_autosave(mut self, root: &Path) -> crate::Result<Self> {
        let in_process =
            || crate::Error::BadArgs("--autosave is for MCP with the project in this process (headless or --ui); the bridged app owns its auto-saves".into());
        let prefs = self.backend.session_prefs().ok_or_else(in_process)?;
        let autosave = AutoSave::new(root, &prefs).map_err(|e| crate::Error::Other(format!("MCP auto-save: {e}")))?;
        let folder = autosave.folder().to_path_buf();
        // Queued for an app in this process: it runs once the app started, before any request.
        self.backend
            .queue_on_session(Box::new(move |s| {
                s.autosave.background = false;
                s.autosave_folder_override = Some(folder);
            }))
            .ok_or_else(in_process)??;
        self.autosave = Some(autosave);
        Ok(self)
    }

    /// Throttle checkpoints of at least `bytes` to one per `interval` (see [`AutoSave`]).
    #[cfg(test)]
    pub(crate) fn throttle_autosave(&mut self, bytes: u64, interval: std::time::Duration) {
        self.autosave = self.autosave.take().map(|a| a.with_throttle(bytes, interval));
    }

    fn autosave_info(&self) -> Value {
        self.autosave.as_ref().map(AutoSave::info).unwrap_or_else(|| json!({"enabled": false}))
    }

    fn checkpoint(&mut self, finish: bool) -> std::io::Result<()> {
        let Some(a) = &mut self.autosave else { return Ok(()) };
        if let Some(s) = self.backend.session() {
            return a.checkpoint(s, finish);
        }
        // An app in this process (`mcp --ui`): checkpoint on its UI thread, between frames.
        let mut job = a.clone();
        match self.backend.with_session(move |s| {
            let r = job.checkpoint(s, finish);
            (job, r)
        }) {
            Some(Ok((done, r))) => {
                *a = done;
                r
            }
            Some(Err(e)) => Err(a.failed(e)),
            None => Ok(()),
        }
    }

    /// Run every tool call as `client` (`mcp --client NAME`): the agent gets its own selection,
    /// active comp and time, and can claim comps. A call's own `client` argument overrides it.
    pub fn with_client(mut self, client: Option<String>) -> Self {
        self.backend.set_client(client);
        self
    }

    pub fn backend(&mut self) -> &mut Backend {
        &mut self.backend
    }

    /// Handle one decoded message (request, notification or batch); `None` when no reply is due.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        if let Value::Array(batch) = msg {
            if batch.is_empty() {
                return Some(error(Value::Null, INVALID_REQUEST, "empty batch"));
            }
            let out: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
            return (!out.is_empty()).then_some(Value::Array(out));
        }
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response from the client (we send no requests) or garbage.
            return msg
                .get("id")
                .filter(|_| msg.get("result").is_none() && msg.get("error").is_none())
                .map(|id| error(id.clone(), INVALID_REQUEST, "missing `method`"));
        };
        let Some(id) = msg.get("id").cloned() else {
            return None; // notification (notifications/initialized, notifications/cancelled, ...)
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let result = self.request(method, &params);
        if method != "tools/call" {
            self.notify.after_request(&mut self.backend);
        }
        Some(match result {
            Ok(mut result) => {
                modern_result_fields(method, &params, &mut result);
                json!({"jsonrpc": "2.0", "id": id, "result": result})
            }
            Err((code, m)) => error(id, code, &m),
        })
    }

    /// Handle one line of input; `None` when no reply is due.
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(msg) => self.handle(&msg)?,
            Err(e) => error(Value::Null, PARSE_ERROR, &format!("parse error: {e}")),
        };
        Some(reply.to_string())
    }

    /// Serve until EOF on `input`. Input is read on its own thread so a long call (a render, an
    /// analysis, a script) can report progress, be cancelled and let other requests through
    /// while it runs (`long_job`). The reader thread is detached: when writing a reply fails, the
    /// server returns at once instead of waiting for a read that may never complete.
    pub fn serve(&mut self, input: impl BufRead + Send + 'static, mut output: impl Write) -> std::io::Result<()> {
        let (tx, inbox) = std::sync::mpsc::channel::<Option<String>>();
        // A read error ends the input like EOF and is returned once the queued requests are answered.
        let read_error = std::sync::Arc::new(std::sync::Mutex::new(None::<std::io::Error>));
        let reader_error = read_error.clone();
        std::thread::Builder::new().name("mcp-input".into()).spawn(move || {
            for line in input.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(e) => {
                        *reader_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                        break;
                    }
                };
                if tx.send(Some(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        })?;
        let served = (|| {
            let mut inbox = crate::long_job::Inbox::new(inbox);
            while let Some(line) = inbox.next() {
                let bridge = self.backend.is_bridge();
                let long = serde_json::from_str::<Value>(line.trim()).ok().and_then(|m| crate::long_job::long_call(bridge, &m));
                if let Some(call) = long {
                    self.run_long(call, &mut inbox, &mut output)?;
                    continue;
                }
                if let Some(reply) = self.handle_line(&line) {
                    output.write_all(reply.as_bytes())?;
                    output.write_all(b"\n")?;
                    output.flush()?;
                }
                self.flush_notifications(&mut output)?;
            }
            match read_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })();
        match (served, self.checkpoint(true)) {
            (r, Ok(())) => r,
            (Ok(()), Err(e)) => Err(e),
            (Err(transport), Err(save)) => Err(std::io::Error::other(format!("{transport}; {save}"))),
        }
    }

    /// Serve on stdin/stdout (the MCP stdio transport).
    pub fn serve_stdio(&mut self) -> std::io::Result<()> {
        self.serve(std::io::BufReader::new(std::io::stdin()), std::io::stdout().lock())
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
                let version =
                    PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS.first().copied().unwrap_or(STRUCTURED_SINCE));
                self.version = Some(version);
                let mode = self.backend.mode();
                // Bridged to an app others use too: run as a named agent unless the server was
                // given a name, so this agent never moves the user's selection, comp or time.
                if mode == "bridge" && self.backend.client().is_none() {
                    self.backend.set_client(Some(default_client(params)));
                }
                let instructions = if self.autosave.is_some() {
                    format!("{INSTRUCTIONS} Headless auto-save and recovery: {}", self.autosave_info())
                } else {
                    INSTRUCTIONS.to_string()
                };
                let instructions = match self.backend.client() {
                    Some(c) if mode == "bridge" => format!(
                        "{instructions} You are the agent `{c}`: every tool call runs as `{c}`; pass `client` to use another name, or `\"client\": \"\"` for a call that should change what the user sees (as the user)."
                    ),
                    _ => instructions,
                };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false, "subscribe": true}, "prompts": {"listChanged": false}, "logging": {}},
                    "serverInfo": {"name": "effectcraft", "title": format!("EffectCraft ({mode})"), "version": env!("CARGO_PKG_VERSION"), "build": effectcraft_engine::build_info::build()},
                    "cliBuild": effectcraft_engine::build_info::build(),
                    "processInstance": effectcraft_engine::build_info::process_instance(),
                    "protocolFeatures": effectcraft_engine::build_info::protocol_features(),
                    "instructions": instructions,
                    "_meta": {"effectcraftAutoSave": self.autosave_info(), "effectcraftBuild": effectcraft_engine::build_info::build()},
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                let structured = self.structured(params);
                let tools: Vec<Value> = tools::available(self.backend.is_bridge())
                    .filter(|t| self.all_tools || tools::alias_of(t.name).is_none())
                    .map(|t| {
                        let mut d = t.descriptor();
                        if let Some(canonical) = tools::alias_of(t.name)
                            && let Some(o) = d.as_object_mut()
                        {
                            o.insert("description".into(), json!(format!("Alias of {canonical} (kept for older workflows). {}", t.description)));
                        }
                        if let Some(schema) = tools::output_schema(t.name).filter(|_| structured)
                            && let Some(o) = d.as_object_mut()
                        {
                            o.insert("outputSchema".into(), schema);
                        }
                        d
                    })
                    .collect();
                Ok(json!({"tools": tools}))
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing tool `name`".to_string()))?;
                let mut args = params.get("arguments").cloned().unwrap_or(json!({}));
                // Idempotency key: `_meta.requestId` or a `request_id` argument (never passed on).
                let request_id = take_request_id(params, &mut args);
                if let Some(cached) = request_id.as_ref().and_then(|rid| self.replies.iter().find(|(k, _)| k == rid)) {
                    return Ok(cached.1.clone());
                }
                if tools::available(self.backend.is_bridge()).all(|t| t.name != name) {
                    let hint = if tools::find(name).is_some() { format!(" ({})", crate::backend::NEED_BRIDGE) } else { String::new() };
                    return Err((INVALID_PARAMS, format!("unknown tool `{name}`{hint}")));
                }
                if let Some(t) = tools::find(name) {
                    t.normalize_args(&mut args);
                }
                if let Some(m) = tools::find(name).and_then(|t| t.unknown_arg(&args)) {
                    return Err((INVALID_PARAMS, m));
                }
                let r = self.guarded(name, &args);
                let structured = self.structured(params);
                let result = self.finish_call(name, r, structured);
                if let Some(rid) = request_id {
                    while self.replies.len() >= REPLY_CACHE {
                        self.replies.pop_front();
                    }
                    self.replies.push_back((rid, result.clone()));
                }
                Ok(result)
            }
            "resources/subscribe" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing `uri`".to_string()))?;
                self.notify.subscribe(&mut self.backend, uri).map_err(uri_error)?;
                Ok(json!({}))
            }
            "resources/unsubscribe" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing `uri`".to_string()))?;
                self.notify.unsubscribe(uri);
                Ok(json!({}))
            }
            "logging/setLevel" => {
                let level = params.get("level").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing `level`".to_string()))?;
                self.notify.set_level(level).map_err(|m| (INVALID_PARAMS, m))?;
                Ok(json!({}))
            }
            "resources/list" => Ok(json!({"resources": resources::list()})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": resources::templates(self.structured(params))})),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing `uri`".to_string()))?;
                let target = resources::parse(uri).map_err(uri_error)?;
                let (tool, args) = target.tool();
                match self.guarded(tool, &args) {
                    Ok(Reply::Json(v)) => Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": json_text(&v)}]})),
                    Ok(Reply::Image { png, .. }) => Ok(json!({"contents": [{"uri": uri, "mimeType": "image/png", "blob": base64::encode(&png)}]})),
                    // A templated comp, layer or command that doesn't exist is "not found".
                    Err(Error::BadArgs(e) | Error::App(e)) if !target.is_document_or_catalog() => Err((RESOURCE_NOT_FOUND, e)),
                    Err(Error::Engine(e)) if !target.is_document_or_catalog() => Err((RESOURCE_NOT_FOUND, e.to_string())),
                    Err(e) => Err((INTERNAL_ERROR, e.to_string())),
                }
            }
            "prompts/list" => Ok(prompts::list(self.structured(params))),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing prompt `name`".to_string()))?;
                prompts::get(name, params.get("arguments")).map_err(|m| (INVALID_PARAMS, m))
            }
            m => Err((METHOD_NOT_FOUND, format!("method not found: {m}"))),
        }
    }
}

/// Remove and return a `tools/call`'s idempotency key: `params._meta.requestId`, or a
/// `request_id` / `requestId` argument (strings or numbers).
fn take_request_id(params: &Value, args: &mut Value) -> Option<String> {
    let key = |v: Option<Value>| match v? {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let from_args = args.as_object_mut().and_then(|a| {
        let rid = a.remove("request_id");
        let camel = a.remove("requestId");
        key(rid).or(key(camel))
    });
    key(params.pointer("/_meta/requestId").cloned()).or(from_args)
}

/// Keep completeness and continuation information visible before large payloads. This only
/// changes object key order (including nested comps); structured data and parsed meaning stay identical.
pub(crate) fn json_text(value: &Value) -> String {
    const HEADER_KEYS: &[&str] = &[
        "ok",
        "error",
        "failed",
        "truncated",
        "offset",
        "limit",
        "total",
        "returned",
        "next_offset",
        "max_chars",
        "compact",
        "complete",
        "omitted",
        "omitted_fields",
        "$omitted",
        "editable",
        "pointer",
        "fetch",
        "comp",
        "revision",
        "time",
        "timeBase",
        "layer",
        "path",
    ];
    fn append(value: &Value, text: &mut String, depth: usize) {
        // Inspection trees are depth-bounded; cap this extra ordering walk too. Deeper legacy
        // payloads keep their ordinary serde representation, without losing any fields.
        if depth >= 64 {
            text.push_str(&value.to_string());
            return;
        }
        match value {
            Value::Object(object) => {
                let headers = HEADER_KEYS.iter().filter_map(|key| object.get(*key).map(|value| (*key, value)));
                let fields =
                    object.iter().filter(|(key, value)| !HEADER_KEYS.contains(&key.as_str()) && !value.is_array()).map(|(key, value)| (key.as_str(), value));
                let lists =
                    object.iter().filter(|(key, value)| !HEADER_KEYS.contains(&key.as_str()) && value.is_array()).map(|(key, value)| (key.as_str(), value));
                text.push('{');
                for (index, (key, value)) in headers.chain(fields).chain(lists).enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    text.push_str(&Value::String(key.into()).to_string());
                    text.push(':');
                    append(value, text, depth + 1);
                }
                text.push('}');
            }
            Value::Array(values) => {
                text.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    append(value, text, depth + 1);
                }
                text.push(']');
            }
            _ => text.push_str(&value.to_string()),
        }
    }
    let mut text = String::new();
    append(value, &mut text, 0);
    text
}

/// A `tools/call` result: tool failures are reported in-band (`isError`) so the model can react.
pub fn call_result(r: Result<Reply, Error>) -> Value {
    call_result_with(r, false)
}

/// [`call_result`]; `structured` (MCP 2025-06-18+) also returns the JSON as `structuredContent`
/// (an image's metadata for image replies), keeping the text content for older clients.
pub fn call_result_with(r: Result<Reply, Error>, structured: bool) -> Value {
    let (mut result, data) = match r {
        Ok(Reply::Json(v)) => {
            let failed = v.get("ok") == Some(&Value::Bool(false))
                || v.get("error").is_some_and(|error| !error.is_null())
                || v.get("failed").and_then(Value::as_u64).is_some_and(|n| n > 0);
            (json!({"content": [{"type": "text", "text": json_text(&v)}], "isError": failed}), Some(v))
        }
        Ok(Reply::Image { png, info }) => (
            json!({
                "content": [
                    {"type": "image", "data": base64::encode(&png), "mimeType": "image/png"},
                    {"type": "text", "text": json_text(&info)},
                ],
                "isError": false,
            }),
            Some(info),
        ),
        Err(e) => (json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true}), None),
    };
    if let Some(v) = data.filter(|_| structured)
        && let Some(o) = result.as_object_mut()
    {
        o.insert("structuredContent".into(), tools::structured(v));
    }
    result
}

impl McpServer {
    /// Run a tool with panics caught: a panic becomes an error result and the session keeps
    /// serving (docs/mcp.md).
    pub(crate) fn guarded(&mut self, name: &str, args: &Value) -> Result<Reply, Error> {
        guarded_run(&mut self.backend, name, args)
    }

    /// A tool call's MCP result, after an auto-save checkpoint (`--autosave`).
    /// A tool call's result for `r`; `structured` for 2025-06-18+ clients. Checkpoints the
    /// project (--autosave) and queues the notifications the call caused.
    pub(crate) fn finish_call(&mut self, name: &str, mut r: Result<Reply, Error>, structured: bool) -> Value {
        // Also checkpoint a partially successful tool that returned an error. The project is
        // durable before the client receives its reply (even if it kills the process
        // immediately afterwards).
        let saved = self.checkpoint(false);
        if self.autosave.is_some()
            && let Ok(Reply::Json(v)) = &mut r
        {
            let info = self.autosave_info();
            let target = match name {
                "get_project" => v.as_object_mut(),
                "doc_inspect" => v.get_mut("project").and_then(Value::as_object_mut),
                _ => None,
            };
            if let Some(o) = target {
                o.insert("autosave".into(), info);
            }
        }
        let mut result = call_result_with(r, structured);
        if let Err(e) = saved {
            let _ = writeln!(std::io::stderr(), "{e}");
            let text = format!("Warning: {e}. Use save_project with a writable path before disconnecting.");
            if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
                content.push(json!({"type": "text", "text": text}));
            }
            self.notify.log("error", json!({"message": text}));
        }
        let ok = result.get("isError") == Some(&Value::Bool(false));
        self.notify.after_tool(&mut self.backend, name, ok);
        if self.autosave.is_some()
            && let Some(obj) = result.as_object_mut()
        {
            let meta = obj.entry("_meta").or_insert_with(|| json!({}));
            if let Some(meta) = meta.as_object_mut() {
                meta.insert("effectcraftAutoSave".into(), self.autosave_info());
            }
        }
        result
    }
}

/// Run a tool with panics caught (see [`McpServer::guarded`]).
///
/// Every tool takes a `client` argument (the agent's name): the engine commands the call runs carry
/// it, so the agent works on its own selection, active comp and time and can claim comps. It
/// overrides the server's own client (`mcp --client NAME`) for this call.
pub(crate) fn guarded_run(backend: &mut Backend, name: &str, args: &Value) -> Result<Reply, Error> {
    use effectcraft_engine::agents::MAX_CLIENT_LEN;
    let mut args = std::borrow::Cow::Borrowed(args);
    // `Some(None)`: `"client": ""`, this call runs as the user.
    let client = match args.get("client") {
        None | Some(Value::Null) => None,
        Some(Value::String(c)) if c.is_empty() => Some(None),
        Some(Value::String(c)) if c.len() <= MAX_CLIENT_LEN => Some(Some(c.clone())),
        Some(c) => {
            return Err(Error::BadArgs(format!(
                "`client` must be your name (a string of at most {MAX_CLIENT_LEN} bytes; \"\" runs the call as the user), got {c}"
            )));
        }
    };
    if let Some(o) = args.to_mut().as_object_mut() {
        o.remove("client");
    }
    let outer_named = backend.set_client_named(client.is_some());
    let outer = client.map(|c| backend.set_client(c));
    let outer_revision = backend.set_expected_revision(None);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tools::run(backend, name, &args))).unwrap_or_else(|p| {
        let msg = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into());
        Err(Error::Other(format!("internal error: {msg}")))
    });
    backend.set_expected_revision(outer_revision);
    backend.set_client_named(outer_named);
    if let Some(outer) = outer {
        backend.set_client(outer);
    }
    // A call that joined an app (`hosts {join}`) as a one-off name: stay a named agent there.
    if name == "hosts" && backend.mode() == "bridge" && backend.client().is_none() {
        backend.set_client(Some(format!("mcp-{}", std::process::id())));
    }
    r
}

/// The name a bridged agent runs under when none was given: the MCP client's name and this
/// server's process id (`claude-code-4821`), so two agents never share a cursor or claims.
pub(crate) fn default_client(initialize: &Value) -> String {
    let info = initialize.get("clientInfo").and_then(|i| i.get("name")).and_then(Value::as_str).unwrap_or("");
    let mut name: String =
        info.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c.to_ascii_lowercase() } else { '-' }).take(40).collect();
    name = name.trim_matches('-').to_string();
    if name.is_empty() {
        name = "agent".into();
    }
    format!("{name}-{}", std::process::id())
}

/// A resource URI that can't be read, as a JSON-RPC error.
fn uri_error(e: UriError) -> (i64, String) {
    match e {
        UriError::NotFound(m) => (RESOURCE_NOT_FOUND, m),
        UriError::Invalid(m) => (INVALID_PARAMS, m),
    }
}

/// Whether a request declares MCP 2026-07-28 or later in its `_meta` (a "modern" client, which
/// negotiates per request instead of through `initialize`).
pub(crate) fn is_modern(params: &Value) -> bool {
    params.pointer("/_meta/io.modelcontextprotocol~1protocolVersion").and_then(Value::as_str).is_some_and(|v| v >= "2026-07-28")
}

/// Modern clients reject list/read results without `resultType`, `ttlMs` and `cacheScope`; those
/// fields are added for them. Sessions that negotiated an older revision through `initialize` keep
/// the legacy shape. The tool and resource lists never change while the server runs (cacheable
/// for ten minutes); reads follow the project, so they are never cached.
pub(crate) fn modern_result_fields(method: &str, params: &Value, result: &mut Value) {
    let ttl_ms = match method {
        "tools/list" | "resources/list" | "resources/templates/list" | "prompts/list" => Some(600_000),
        "resources/read" => Some(0),
        "tools/call" => None,
        _ => return,
    };
    let Some(obj) = result.as_object_mut().filter(|_| is_modern(params)) else { return };
    obj.insert("resultType".into(), json!("complete"));
    if let Some(ttl_ms) = ttl_ms {
        obj.insert("ttlMs".into(), json!(ttl_ms));
        obj.insert("cacheScope".into(), json!("private"));
    }
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod result_tests {
    use super::*;

    fn text_json(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn script_server() -> McpServer {
        let mut session = crate::Session::default();
        effectcraft_script::install(&mut session);
        let mut server = McpServer::new(Backend::headless(session));
        server.handle(&json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18"}}));
        server
    }

    fn call(server: &mut McpServer, name: &str, arguments: Value) -> Value {
        server.handle(&json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":name, "arguments":arguments}})).unwrap()["result"].clone()
    }

    #[test]
    fn json_failure_signals_preserve_the_entire_payload() {
        for payload in [
            json!({"ok":false,"error":null,"output":"before failure","partial":{"comp":7}}),
            json!({"ok":true,"error":{"message":"failure"},"output":"details"}),
            json!({"failed":1,"results":[{"ok":true},{"ok":false}]}),
        ] {
            for structured in [false, true] {
                let result = call_result_with(Ok(Reply::Json(payload.clone())), structured);
                assert_eq!(result["isError"], true, "{result}");
                assert_eq!(text_json(&result), payload);
                if structured {
                    assert_eq!(result["structuredContent"], tools::structured(payload.clone()));
                }
            }
        }
        for payload in [json!({"ok":true,"error":null}), json!({"failed":0}), json!({"value":false})] {
            assert_eq!(call_result(Ok(Reply::Json(payload)))["isError"], false);
        }
    }

    #[test]
    fn thrown_script_error_and_prior_output_reach_mcp() {
        let mut server = script_server();
        let result = call(&mut server, "run_script", json!({"code":"$.writeln('before throw'); throw new Error('broken recipe');"}));
        assert_eq!(result["isError"], true, "{result}");
        let data = text_json(&result);
        assert_eq!(data["ok"], false);
        assert!(data["error"]["message"].as_str().unwrap().contains("broken recipe"), "{data}");
        assert!(data["output"].as_str().unwrap().contains("before throw"), "{data}");
        assert_eq!(result["structuredContent"], tools::structured(data));
    }

    #[test]
    fn stopped_script_reports_rollback_and_kept_partial_edits() {
        let mut server = script_server();
        // A stopped script undoes all its edits; with rollback: false only an open undo group.
        for (group, name, kept, rollback) in
            [("app.beginUndoGroup('Build');", "RolledBack", false, true), ("", "AllRolledBack", false, true), ("", "KeptPartial", true, false)]
        {
            let code = format!("{group} app.project.items.addComp('{name}', 64, 64, 1, 1, 25); $.writeln('before timeout'); while (true) {{}}");
            let started = std::time::Instant::now();
            let result = call(&mut server, "run_script", json!({"code":code,"timeout":0.3,"rollback":rollback}));
            assert!(started.elapsed() < std::time::Duration::from_secs(3));
            assert_eq!(result["isError"], true, "{result}");
            let data = text_json(&result);
            assert_eq!(data["ok"], false);
            assert!(data["error"]["message"].as_str().unwrap().contains("time limit"), "{data}");
            let output = data["output"].as_str().unwrap();
            assert!(output.contains("before timeout"), "{data}");
            assert!(output.contains(if kept { "are kept" } else { "olled back" }), "{data}");
            assert_eq!(result["structuredContent"], tools::structured(data));
            let project = call(&mut server, "get_project", json!({}));
            assert_eq!(text_json(&project).to_string().contains(name), kept, "{project}");
        }
        let result = call(&mut server, "run_script", json!({"code":"1 + 1"}));
        assert_eq!(result["isError"], false);
        assert_eq!(text_json(&result)["result"], 2);
    }

    #[test]
    fn model_text_has_paging_headers_before_long_lists_without_changing_json() {
        let value = json!({"commands":["x".repeat(8000)], "next_offset":1, "truncated":true, "total":2, "offset":0,
            "returned":1, "max_chars":7000, "revision":17, "timeBase":"comp", "name":"quoted \" and 🧪", "a\"key":true});
        let text = json_text(&value);
        let list = text.find("\"commands\":").unwrap();
        for key in ["next_offset", "truncated", "total", "offset", "returned", "max_chars", "revision", "timeBase"] {
            assert!(text.find(&format!("\"{key}\":")).unwrap() < list, "{key}");
        }
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
        assert_eq!(text.len(), value.to_string().len());
        assert_eq!(text_json(&call_result(Ok(Reply::Json(value.clone())))), value);
        let image = call_result(Ok(Reply::Image { png: vec![], info: value.clone() }));
        assert_eq!(serde_json::from_str::<Value>(image["content"][1]["text"].as_str().unwrap()).unwrap(), value);
        for scalar in [json!(null), json!([1, 2]), json!("escaped\ntext"), json!({})] {
            assert_eq!(serde_json::from_str::<Value>(&json_text(&scalar)).unwrap(), scalar);
        }
    }

    #[test]
    fn nested_doc_inspect_comp_headers_precede_layers() {
        let value = json!({"revision":17, "project":{"name":"Doc"}, "activeComp":{
            "comp":9, "revision":17, "total":110, "returned":1, "next_offset":1, "truncated":{"field":"layers"},
            "layers":[{"name":"🧪".repeat(2000),"properties":{"keys":[1,2],"total":2,"returned":2,"next_offset":null}}]
        }});
        let text = json_text(&value);
        let comp = &text[text.find("\"activeComp\":{").unwrap()..];
        let layers = comp.find("\"layers\":[").unwrap();
        for key in ["comp", "revision", "total", "returned", "next_offset", "truncated"] {
            assert!(comp.find(&format!("\"{key}\":")).unwrap() < layers, "{key}");
        }
        let properties = &comp[comp.find("\"properties\":{").unwrap()..];
        assert!(properties.find("\"returned\":").unwrap() < properties.find("\"keys\":[").unwrap());
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
        assert_eq!(text.len(), value.to_string().len());
        let mut deep = json!({"keys":[1,2],"total":2});
        for _ in 0..70 {
            deep = json!({"child":deep});
        }
        assert_eq!(serde_json::from_str::<Value>(&json_text(&deep)).unwrap(), deep);
        assert_eq!(json_text(&deep).len(), deep.to_string().len());
    }
}

#[cfg(test)]
mod build_identity_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct OfflineApp(Arc<AtomicUsize>);

    impl crate::ControlTransport for OfflineApp {
        fn call(&mut self, _: &str, _: Value) -> crate::Result<Value> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(Error::Other("app unavailable".into()))
        }
    }

    #[test]
    fn initialize_describes_cli_build_without_contacting_the_app() {
        let requests = Arc::new(AtomicUsize::new(0));
        let backend = Backend::transport(OfflineApp(requests.clone()));
        let mut server = McpServer::new(backend);
        let reply = server.handle(&json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18"}})).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        let result = &reply["result"];
        assert_eq!(result["cliBuild"], effectcraft_engine::build_info::build());
        assert_eq!(result["serverInfo"]["build"], result["cliBuild"]);
        assert_eq!(result["processInstance"], json!(effectcraft_engine::build_info::process_instance()));
        assert_eq!(result["protocolFeatures"]["sessionIdentity"], true);
        assert_eq!(requests.load(Ordering::Relaxed), 0, "initialize must work with an offline app");
    }
}
