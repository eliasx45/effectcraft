//! Immutable retained tool results. Continuations read a snapshot and never rerun an edit.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::{Error, Result, compact};

const MAX_ENTRIES: usize = 32;
const MEMORY_BYTES: usize = 64 * 1024 * 1024;
const MEMORY_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const DISK_BYTES: usize = 256 * 1024 * 1024;
const METADATA_RESERVE: usize = 512;
const RETENTION_MESSAGE: &str = "The action finished; its complete result is unavailable. Do not rerun an edit to fetch it.";
const PENDING_RETENTION_MESSAGE: &str = "The operation has not completed; its result is unavailable. Do not rerun the edit to fetch it.";

#[derive(Default)]
pub(crate) struct ResultStore {
    entries: VecDeque<Entry>,
    memory: usize,
    disk: usize,
    directory: Option<PathBuf>,
}

struct Entry {
    id: String,
    client: Option<String>,
    source: String,
    failed: bool,
    identity: Value,
    bytes: usize,
    stored: Stored,
}

enum Stored {
    Memory(Value),
    File { path: PathBuf, total: usize },
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Stored::File { path, .. } = &self.stored {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for ResultStore {
    fn drop(&mut self) {
        self.entries.clear();
        if let Some(path) = &self.directory {
            let _ = std::fs::remove_dir(path);
        }
    }
}

fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| Error::Other(format!("cannot create retained result id: {e}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn failed(value: &Value) -> bool {
    value.get("ok") == Some(&Value::Bool(false))
        || value.get("error").is_some_and(|v| !v.is_null())
        || value.get("failed").and_then(Value::as_u64).is_some_and(|n| n > 0)
}

fn operation_completed(value: &Value) -> bool {
    value.get("operation_completed").and_then(Value::as_bool) != Some(false)
        && !["waiting", "running"].iter().any(|key| value.get(*key).and_then(Value::as_bool) == Some(true))
}

/// Conservative live allocations, including spare vector/string capacity and object overhead.
fn memory_size(value: &Value, depth: usize) -> Option<usize> {
    if depth > 160 {
        return None;
    }
    let base = std::mem::size_of::<Value>();
    match value {
        Value::String(s) => base.checked_add(s.capacity()),
        Value::Array(items) => items.iter().try_fold(base.checked_add(items.capacity().checked_mul(base)?)?, |n, v| n.checked_add(memory_size(v, depth + 1)?)),
        Value::Object(fields) => {
            fields.iter().try_fold(base, |n, (k, v)| n.checked_add(64)?.checked_add(k.capacity())?.checked_add(memory_size(v, depth + 1)?))
        }
        _ => Some(base),
    }
}

fn identity(value: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "comp",
        "revision",
        "id",
        "layer",
        "time",
        "timeBase",
        "ok",
        "failed",
        "completed",
        "success",
        "warnings",
        "error",
        "waiting",
        "running",
        "operation_completed",
    ] {
        if let Some(v) = value.get(key) {
            // Large error/warning payloads remain explicit references, never silently disappear.
            let pointer = format!("/{key}");
            if let Ok(v) = compact::bounded_from_ref(v, &pointer, 128) {
                out.insert(key.into(), v);
            }
        }
    }
    if failed(value) {
        out.insert("ok".into(), json!(false));
    }
    if !operation_completed(value) {
        out.insert("operation_completed".into(), json!(false));
    }
    Value::Object(out)
}

fn reference_route(value: &mut Value, fetch: &str, spilled: bool, depth: usize) {
    if depth > 64 {
        return;
    }
    match value {
        Value::Object(fields) => {
            if fields.get("$omitted").and_then(Value::as_bool) == Some(true) {
                fields.insert("fetch".into(), json!(fetch));
                if spilled {
                    fields.insert("pointer".into(), json!(""));
                }
            } else {
                for child in fields.values_mut() {
                    reference_route(child, fetch, spilled, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                reference_route(child, fetch, spilled, depth + 1);
            }
        }
        _ => {}
    }
}

fn array_pointer(value: &Value) -> Option<String> {
    if value.is_array() {
        return Some(String::new());
    }
    let fields = value.as_object()?;
    for key in ["items", "results", "states", "undoStack", "undo", "commands", "effects", "fonts", "output", "logs"] {
        if fields.get(key).is_some_and(Value::is_array) {
            return Some(format!("/{key}"));
        }
    }
    fields.iter().find(|(key, v)| key.len() <= 128 && v.is_array()).map(|(key, _)| format!("/{}", key.replace('~', "~0").replace('/', "~1")))
}

/// Build from borrowed rows so a giant saved field is never cloned to make a small page.
fn page(value: &Value, pointer: &str, a: &Value, cap: usize) -> Result<Value> {
    let items = value
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::BadArgs("page mode requires a JSON pointer to an array; use mode: json for an exact field".into()))?;
    let request = compact::Page::from_args(a, 100)?;
    let start = request.offset.min(items.len());
    let field = pointer.strip_prefix('/').filter(|s| !s.contains('/')).map(|s| s.replace("~1", "/").replace("~0", "~"));
    let mut out = if let Some(field) = &field {
        let mut out = compact::bounded_from_ref(value, "", cap / 3)?;
        if out.get("$omitted").and_then(Value::as_bool) == Some(true) {
            out = json!({"header": out});
        }
        if let Some(fields) = out.as_object_mut() {
            fields.remove(field);
        }
        out
    } else {
        json!({})
    };
    let field = field.as_deref().unwrap_or("items");
    out["offset"] = json!(start);
    out["total"] = json!(items.len());
    out["returned"] = json!(0);
    out["next_offset"] = json!(items.len());
    out["page_pointer"] = json!(pointer);
    let used = compact::source_json_len(&out)?.saturating_add(100);
    let available = cap.saturating_sub(used);
    let mut rows = Vec::new();
    let mut bytes = 2usize;
    for (index, item) in items.iter().enumerate().skip(start).take(request.limit) {
        let row_pointer = format!("{pointer}/{index}");
        let remaining = available.saturating_sub(bytes).saturating_sub(1);
        // Only the first row may need field references; otherwise continue on the next page.
        let row = if rows.is_empty() {
            compact::bounded_from_ref(item, &row_pointer, remaining)
        } else if compact::fits_budget(item, remaining) {
            Ok(item.clone())
        } else {
            break;
        };
        let row = match row {
            Ok(row) => row,
            Err(_) => break,
        };
        let row_bytes = compact::source_json_len(&row)?;
        if row_bytes.saturating_add(bytes).saturating_add(1) > available {
            break;
        }
        bytes = bytes.saturating_add(row_bytes).saturating_add(1);
        rows.push(row);
    }
    if rows.is_empty() && start < items.len() {
        return Err(Error::BadArgs("reply budget cannot fit one saved-result row; increase max_chars".into()));
    }
    let returned = rows.len();
    let next = start.saturating_add(returned);
    out[field] = Value::Array(rows);
    out["returned"] = json!(returned);
    out["next_offset"] = json!((next < items.len()).then_some(next));
    out["truncated"] = json!(next < items.len());
    Ok(out)
}

impl ResultStore {
    /// A final outward tool result. Existing compact readers and get_result are already bounded.
    pub(crate) fn finish(&mut self, name: &str, a: &Value, value: Value, client: Option<&str>) -> Result<Value> {
        if (a.get("full").and_then(Value::as_bool) == Some(true) && !compact::Page::requested(a)) || matches!(name, "get_result" | "get_field") {
            return Ok(value);
        }
        let cap = match compact::budget(a) {
            Ok(cap) => cap,
            Err(e) => return self.retention_failure(&value, compact::DEFAULT_MAX_CHARS, &e.to_string()),
        };
        let already_paged = value.get("max_chars").is_some() && value.get("returned").is_some();
        let already_retained = value.get("result_id").and_then(Value::as_str).is_some_and(|id| self.entries.iter().any(|entry| entry.id == id));
        if compact::fits_budget(&value, cap) && (!compact::Page::requested(a) || already_paged || already_retained) {
            return Ok(value);
        }
        let total = match compact::source_json_len(&value) {
            Ok(total) => total,
            Err(e) => return self.retention_failure(&value, cap, &e.to_string()),
        };
        let Some(memory) = memory_size(&value, 0) else {
            return self.retention_failure(&value, cap, "retained result exceeds the supported memory/depth count");
        };
        let spilled = memory > MEMORY_ENTRY_BYTES;
        let id = match token() {
            Ok(id) => id,
            Err(e) => return self.retention_failure(&value, cap, &e.to_string()),
        };
        let source_identity = identity(&value);
        let identity_bytes = match compact::source_json_len(&source_identity) {
            Ok(bytes) => bytes,
            Err(e) => return self.retention_failure(&value, cap, &e.to_string()),
        };
        let reserved = identity_bytes.saturating_add(256).max(METADATA_RESERVE);
        let preview = if !spilled && let Some(pointer) = array_pointer(&value) {
            page(&value, &pointer, a, cap.saturating_sub(reserved))
        } else {
            compact::bounded_from_ref(&value, "", cap.saturating_sub(reserved))
        };
        let mut preview = match preview {
            Ok(preview) => preview,
            Err(e) => return self.retention_failure(&value, cap, &e.to_string()),
        };
        if !preview.is_object() || value.get("result_id").is_some() {
            preview = json!({"data": preview});
        }
        if preview.get("$omitted").and_then(Value::as_bool) == Some(true) {
            preview = json!({"header": preview});
        }
        if let Some(fields) = preview.as_object_mut()
            && let Some(source) = source_identity.as_object()
        {
            for (key, value) in source {
                if fields.get(key).is_none_or(|v| v.get("$omitted").and_then(Value::as_bool) == Some(true)) {
                    fields.insert(key.clone(), value.clone());
                }
            }
            if failed(&value) {
                fields.insert("ok".into(), json!(false));
            }
            if !operation_completed(&value) {
                fields.insert("operation_completed".into(), json!(false));
            }
        }
        reference_route(&mut preview, "get_result", spilled, 0);
        preview["result_id"] = json!(id);
        preview["result_source"] = json!(name);
        preview["result_mode"] = json!(if preview.get("page_pointer").is_some() { "page" } else { "json" });
        preview["result_pointer"] = preview.get("page_pointer").cloned().unwrap_or_else(|| json!(""));
        preview["max_chars"] = json!(cap);
        if !compact::fits_budget(&preview, cap) {
            return self.retention_failure(&value, cap, "reply status and result references exceed max_chars; increase it");
        }
        let stored = if spilled {
            if total > DISK_BYTES {
                return self.retention_failure(&value, cap, "result exceeds the 256 MiB disk retention budget");
            }
            let path = match self.spill(&id, &value) {
                Ok(path) => path,
                Err(e) => return self.retention_failure(&value, cap, &e.to_string()),
            };
            Stored::File { path, total }
        } else {
            Stored::Memory(value)
        };
        let bytes = if spilled { total } else { memory };
        while self.entries.len() >= MAX_ENTRIES
            || (if spilled { self.disk.saturating_add(bytes) > DISK_BYTES } else { self.memory.saturating_add(bytes) > MEMORY_BYTES })
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            match &entry.stored {
                Stored::Memory(_) => self.memory = self.memory.saturating_sub(entry.bytes),
                Stored::File { .. } => self.disk = self.disk.saturating_sub(entry.bytes),
            }
        }
        if spilled {
            self.disk = self.disk.saturating_add(bytes);
        } else {
            self.memory = self.memory.saturating_add(bytes);
        }
        self.entries.push_back(Entry {
            id,
            client: client.map(str::to_owned),
            source: name.into(),
            failed: source_identity.get("ok") == Some(&Value::Bool(false)),
            identity: source_identity,
            bytes,
            stored,
        });
        Ok(preview)
    }

    fn retention_failure(&self, value: &Value, cap: usize, message: &str) -> Result<Value> {
        // Returning a result does not complete a suspended ScriptUI call or a running job.
        let completed = operation_completed(value);
        let status_message = if completed { RETENTION_MESSAGE } else { PENDING_RETENTION_MESSAGE };
        let mut diagnostic = String::new();
        for ch in message.chars() {
            if diagnostic.len().saturating_add(ch.len_utf8()) > 180 {
                break;
            }
            diagnostic.push(ch);
        }
        if diagnostic.len() < message.len() {
            diagnostic.push_str(" (truncated)");
        }
        let mut out = json!({
            "ok": false, "operation_completed": completed,
            "waiting": value.get("waiting").and_then(Value::as_bool), "running": value.get("running").and_then(Value::as_bool),
            "source_ok": value.get("ok").and_then(Value::as_bool), "source_failed": failed(value),
            "result_id": null, "result_retained": false, "result_retention_error": diagnostic,
            "message": status_message
        });
        // Keep original diagnostics/identity where they fit. Large values are explicitly unavailable.
        for (key, v) in identity(value).as_object().into_iter().flat_map(|fields| fields.iter()) {
            if matches!(key.as_str(), "ok" | "operation_completed" | "waiting" | "running") {
                continue;
            }
            let mut candidate = out.clone();
            candidate[key] = v.clone();
            if compact::fits_budget(&candidate, cap.saturating_sub(128)) {
                out = candidate;
            }
        }
        let used = compact::source_json_len(&out).unwrap_or(cap);
        if let Ok(payload) = compact::bounded_from_ref(value, "", cap.saturating_sub(used).saturating_sub(16)) {
            if payload.is_object() && payload.get("$omitted").is_none() {
                if let (Some(fields), Some(payload)) = (out.as_object_mut(), payload.as_object()) {
                    for (key, value) in payload {
                        fields.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            } else {
                out["outcome"] = payload;
            }
        }
        reference_route(&mut out, "unavailable", false, 0);
        // Malformed/deep results and storage errors are delivery failures, never panics.
        // Keep the no-rerun instruction literal even when the outcome needs a minimal header.
        if !compact::fits_budget(&out, cap) {
            out = json!({"ok": false, "operation_completed": completed,
                "waiting": value.get("waiting").and_then(Value::as_bool), "running": value.get("running").and_then(Value::as_bool), "source_ok": value.get("ok").and_then(Value::as_bool),
                "source_failed": failed(value), "result_id": null, "result_retained": false,
                "message": status_message, "result_retention_error": diagnostic,
                "error": "Result delivery failed after the operation returned its current state."});
        }
        Ok(out)
    }

    fn spill(&mut self, id: &str, value: &Value) -> Result<PathBuf> {
        if self.directory.is_none() {
            let path = std::env::temp_dir().join(format!("effectcraft-results-{}", token()?));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&path).map_err(|e| Error::Other(format!("cannot create result cache: {e}")))?;
            self.directory = Some(path);
        }
        let directory = self.directory.as_ref().ok_or_else(|| Error::Other("missing result cache directory".into()))?;
        let path = directory.join(format!("{id}.json"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|e| Error::Other(format!("cannot retain result: {e}")))?;
        let mut writer = std::io::BufWriter::with_capacity(16 * 1024, file);
        let written = serde_json::to_writer(&mut writer, value).map_err(std::io::Error::other).and_then(|()| writer.flush());
        drop(writer);
        if let Err(e) = written {
            let _ = std::fs::remove_file(&path);
            return Err(Error::Other(format!("cannot retain result: {e}")));
        }
        Ok(path)
    }

    /// A retained result by its id. The id is an unguessable token handed to whoever made the
    /// result; a call that names a `client` (`named`) only gets that client's results, a call
    /// without one gets any (a sub-agent that passed `client` to run_script but not to
    /// get_result).
    pub(crate) fn fetch(&self, a: &Value, client: Option<&str>, named: bool) -> Result<Value> {
        let id = a.get("result_id").and_then(Value::as_str).ok_or_else(|| Error::BadArgs("missing result_id".into()))?;
        let entry =
            self.entries.iter().find(|e| e.id == id).ok_or_else(|| {
                Error::BadArgs("result id is unknown or expired (only the last 32 results are kept); fetching never reruns its source".into())
            })?;
        if named && entry.client.as_deref() != client {
            let owner = entry.client.as_deref().map_or("the user (\"client\": \"\")".to_string(), |c| format!("client `{c}`"));
            return Err(Error::BadArgs(format!("result {id} belongs to {owner}: pass that client, or none")));
        }
        let cap = compact::budget(a)?;
        let pointer = a.get("pointer").and_then(Value::as_str).unwrap_or("");
        let default_mode =
            if a.get("pointer").is_none() && matches!(&entry.stored, Stored::Memory(value) if array_pointer(value).is_some()) { "page" } else { "json" };
        let mode = a.get("mode").and_then(Value::as_str).unwrap_or(default_mode);
        let mut out = match (&entry.stored, mode) {
            (Stored::Memory(value), "json") => compact::field_chunk_with_budget(value, pointer, a, cap.saturating_sub(METADATA_RESERVE))?,
            (Stored::Memory(value), "page") => {
                let pointer = if a.get("pointer").is_none() { array_pointer(value).unwrap_or_default() } else { pointer.into() };
                page(value, &pointer, a, cap.saturating_sub(METADATA_RESERVE))?
            }
            (Stored::File { path, total }, "json") if pointer.is_empty() => file_chunk(path, *total, a, cap.saturating_sub(METADATA_RESERVE))?,
            (Stored::File { .. }, "page" | "json") => {
                return Err(Error::BadArgs(
                    "large result is stored as streaming JSON; fetch mode: json, pointer: \"\" to recover the complete immutable result".into(),
                ));
            }
            _ => return Err(Error::BadArgs("mode must be page or json".into())),
        };
        reference_route(&mut out, "get_result", false, 0);
        out["result_id"] = json!(id);
        out["result_source"] = json!(entry.source);
        out["source_failed"] = json!(entry.failed);
        if entry.failed {
            out["ok"] = json!(false);
        }
        for key in ["comp", "revision", "waiting", "running", "operation_completed"] {
            if let Some(v) = entry.identity.get(key) {
                out[key] = v.clone();
            }
        }
        if !compact::fits_budget(&out, cap) {
            return Err(Error::BadArgs("saved result chunk metadata exceeds max_chars; increase it".into()));
        }
        Ok(out)
    }
}

fn file_chunk(path: &std::path::Path, total: usize, a: &Value, cap: usize) -> Result<Value> {
    let request = compact::Page::from_args(a, 3500)?;
    let start = request.offset.min(total);
    let mut end = start.saturating_add(request.limit).min(total);
    let window_start = start.saturating_sub(3);
    let window_end = end.saturating_add(3).min(total);
    let mut bytes = vec![0u8; window_end.saturating_sub(window_start)];
    let mut file = File::open(path).map_err(|e| Error::Other(format!("cannot read retained result: {e}")))?;
    file.seek(SeekFrom::Start(u64::try_from(window_start).map_err(|_| Error::Other("result offset is too large".into()))?))
        .map_err(|e| Error::Other(format!("cannot seek retained result: {e}")))?;
    file.read_exact(&mut bytes).map_err(|e| Error::Other(format!("cannot read retained result chunk: {e}")))?;
    let boundary = |position: usize| position == total || bytes.get(position.saturating_sub(window_start)).is_some_and(|byte| byte & 0xc0 != 0x80);
    if !boundary(start) {
        return Err(Error::BadArgs("offset must lie on a UTF-8 boundary".into()));
    }
    while !boundary(end) {
        end = end.saturating_sub(1);
    }
    loop {
        let bytes = bytes
            .get(start.saturating_sub(window_start)..end.saturating_sub(window_start))
            .ok_or_else(|| Error::Other("invalid retained result chunk".into()))?;
        let text = std::str::from_utf8(bytes).map_err(|e| Error::Other(format!("invalid retained result UTF-8: {e}")))?;
        let out = json!({"pointer": "", "encoding": "json", "text": text, "offset": start, "total": total, "returned": end - start, "next_offset": (end < total).then_some(end)});
        if compact::fits_budget(&out, cap) && (end > start || start == total) {
            return Ok(out);
        }
        if end == start {
            return Err(Error::BadArgs("reply budget or limit cannot fit one JSON character".into()));
        }
        end = end.saturating_sub(1);
        while !boundary(end) {
            end = end.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_results_and_full_compatibility_keep_their_shapes() {
        let mut store = ResultStore::default();
        let value = json!({"ok": true, "value": 1.234567891234});
        assert_eq!(store.finish("run_script", &json!({}), value.clone(), None).unwrap(), value);
        let large = json!(["x".repeat(20_000)]);
        assert_eq!(store.finish("command_list", &json!({"full": true}), large.clone(), None).unwrap(), large);
        assert!(store.entries.is_empty());
    }

    #[test]
    fn saved_arrays_page_without_rerunning_the_source() {
        let mut store = ResultStore::default();
        let original = json!({"revision": 12, "items": (0..500).map(|id| json!({"id": id, "name": "🤖".repeat(20)})).collect::<Vec<_>>()});
        let first = store.finish("get_project", &json!({}), original.clone(), Some("a")).unwrap();
        assert!(compact::fits_budget(&first, 7000));
        assert_eq!(first["total"], 500);
        assert_eq!(first["revision"], 12);
        let id = first["result_id"].as_str().unwrap();
        let mut ids: Vec<u64> = first["items"].as_array().unwrap().iter().map(|r| r["id"].as_u64().unwrap()).collect();
        let mut next = first["next_offset"].as_u64();
        while let Some(offset) = next {
            let reply = store.fetch(&json!({"result_id": id, "mode": "page", "pointer": "/items", "offset": offset}), Some("a"), true).unwrap();
            assert!(compact::fits_budget(&reply, 7000));
            let rows = reply["items"].as_array().unwrap();
            assert!(!rows.is_empty());
            ids.extend(rows.iter().map(|r| r["id"].as_u64().unwrap()));
            next = reply["next_offset"].as_u64();
            if let Some(next) = next {
                assert!(next > offset);
            }
        }
        assert_eq!(ids, (0..500).collect::<Vec<_>>());
        assert_eq!(store.entries.len(), 1);
        assert!(matches!(&store.entries.front().unwrap().stored, Stored::Memory(value) if value == &original));
    }

    #[test]
    fn failure_warning_and_exact_field_values_survive_bounding() {
        let mut store = ResultStore::default();
        let original = json!({"ok": false, "error": "execution failed", "failed": 1, "warnings": ["w".repeat(10_000)], "results": [{"value": 1.234567891234}], "output": "🤖\n".repeat(3000)});
        let reply = store.finish("run_script", &json!({}), original.clone(), None).unwrap();
        assert!(compact::fits_budget(&reply, 7000));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "execution failed");
        assert_eq!(reply["failed"], 1);
        assert_eq!(reply["warnings"]["fetch"], "get_result");
        assert_eq!(reply["warnings"]["pointer"], "/warnings");
        let id = reply["result_id"].as_str().unwrap();
        let exact = store.fetch(&json!({"result_id": id, "pointer": "/results/0/value", "mode": "json", "max_chars": 1024}), None, true).unwrap();
        assert!(compact::fits_budget(&exact, 1024));
        assert_eq!(exact["ok"], false);
        assert_eq!(exact["source_failed"], true);
        assert_eq!(exact["text"], "1.234567891234");
        assert_eq!(store.entries.len(), 1);
    }

    #[test]
    fn continuations_bind_client_and_expire_on_fifo_eviction() {
        let mut store = ResultStore::default();
        let first = store.finish("command_batch", &json!({"limit": 1}), json!({"results": [1, 2]}), Some("a")).unwrap();
        let id = first["result_id"].as_str().unwrap();
        assert!(store.fetch(&json!({"result_id": id}), Some("b"), true).is_err());
        // A call that names no client fetches by the id alone.
        assert!(store.fetch(&json!({"result_id": id}), Some("default"), false).is_ok());
        for _ in 0..MAX_ENTRIES {
            store.finish("command_batch", &json!({"limit": 1}), json!({"results": [1, 2]}), Some("a")).unwrap();
        }
        assert_eq!(store.entries.len(), MAX_ENTRIES);
        assert!(store.fetch(&json!({"result_id": id}), Some("a"), true).is_err());
        assert!(store.memory <= MEMORY_BYTES);
    }

    #[test]
    fn retention_failure_keeps_action_outcome_without_a_fake_fetch_handle() {
        let store = ResultStore::default();
        let out = store.retention_failure(&json!({"ok": true, "output": "x".repeat(100_000)}), 7000, "disk full").unwrap();
        assert!(compact::fits_budget(&out, 7000));
        assert_eq!(out["ok"], false);
        assert_eq!(out["source_ok"], true);
        assert_eq!(out["source_failed"], false);
        assert_eq!(out["result_id"], Value::Null);
        assert_eq!(out["result_retained"], false);
        assert_eq!(out["output"]["fetch"], "unavailable");
        assert!(out["message"].as_str().unwrap().contains("Do not rerun"));
        assert_eq!(out["operation_completed"], true);
    }

    #[test]
    fn post_operation_depth_and_budget_failures_report_completion() {
        let mut store = ResultStore::default();
        let mut deep = json!(1);
        for _ in 0..200 {
            deep = json!({"child": deep});
        }
        let out = store.finish("run_script", &json!({}), json!({"ok": true, "output": deep}), None).unwrap();
        assert_eq!(out["operation_completed"], true);
        assert_eq!(out["source_ok"], true);
        assert_eq!(out["source_failed"], false);
        assert!(out["message"].as_str().unwrap().contains("Do not rerun"));
        assert!(compact::fits_budget(&out, 7000));
        let mut source = json!({"ok": true, "output": "x".repeat(100_000), "warnings": ["w".repeat(10_000)]});
        for key in ["comp", "revision", "id", "layer", "time", "timeBase", "success", "completed"] {
            source[key] = json!("x".repeat(120));
        }
        let out = store.finish("run_script", &json!({"max_chars": 1024}), source, None).unwrap();
        assert_eq!(out["operation_completed"], true);
        assert_eq!(out["source_ok"], true);
        assert!(compact::fits_budget(&out, 1024));
    }

    #[test]
    fn suspended_and_running_sources_are_not_reported_as_completed() {
        for state in [json!({"ok": true, "waiting": true, "running": false}), json!({"ok": true, "running": true})] {
            let mut store = ResultStore::default();
            let mut source = state.clone();
            source["output"] = json!("x".repeat(100_000));
            let preview = store.finish("run_script", &json!({}), source, None).unwrap();
            assert_eq!(preview["operation_completed"], false);
            for key in ["waiting", "running"] {
                if let Some(v) = state.get(key) {
                    assert_eq!(&preview[key], v);
                }
            }
            let id = preview["result_id"].as_str().unwrap();
            let chunk = store.fetch(&json!({"result_id": id, "pointer": "/ok", "mode": "json"}), None, true).unwrap();
            assert_eq!(chunk["operation_completed"], false);
            for key in ["waiting", "running"] {
                if let Some(v) = state.get(key) {
                    assert_eq!(&chunk[key], v);
                }
            }
            let failed = store.retention_failure(&state, 1024, "disk full").unwrap();
            assert_eq!(failed["operation_completed"], false);
            assert_eq!(failed["source_ok"], true);
            assert!(failed["message"].as_str().unwrap().contains("Do not rerun"));
            assert!(!failed["message"].as_str().unwrap().contains("finished"));
            for key in ["waiting", "running"] {
                if let Some(v) = state.get(key) {
                    assert_eq!(&failed[key], v);
                }
            }
            assert!(compact::fits_budget(&failed, 1024));
            let mut deep = json!(1);
            for _ in 0..200 {
                deep = json!({"child": deep});
            }
            let mut source = state.clone();
            source["output"] = deep;
            let failed = store.finish("run_script", &json!({}), source, None).unwrap();
            assert_eq!(failed["operation_completed"], false);
            for key in ["waiting", "running"] {
                if let Some(v) = state.get(key) {
                    assert_eq!(&failed[key], v);
                }
            }
            assert!(failed["message"].as_str().unwrap().contains("Do not rerun"));
        }
    }

    #[test]
    fn repeated_finalization_does_not_page_or_store_again() {
        let mut store = ResultStore::default();
        let args = json!({"offset": 2, "limit": 3});
        let first = store.finish("command_list", &args, json!((0..10).collect::<Vec<_>>()), None).unwrap();
        let again = store.finish("command_list", &args, first.clone(), None).unwrap();
        assert_eq!(again, first);
        assert_eq!(again["items"], json!([2, 3, 4]));
        assert_eq!(store.entries.len(), 1);
    }

    #[test]
    fn malformed_or_missing_spill_files_return_errors_without_panicking() {
        let mut store = ResultStore::default();
        let path = store.spill(&token().unwrap(), &json!(null)).unwrap();
        std::fs::write(&path, [0xff, b'a']).unwrap();
        let error = file_chunk(&path, 2, &json!({}), 512).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
        std::fs::remove_file(&path).unwrap();
        assert!(file_chunk(&path, 2, &json!({}), 512).is_err());
    }

    #[test]
    fn giant_values_spill_and_exact_chunks_use_bounded_memory() {
        let mut store = ResultStore::default();
        let source = json!({"revision": 8, "output": "x".repeat(MEMORY_ENTRY_BYTES + 1)});
        let reply = store.finish("run_script", &json!({}), source, Some("a")).unwrap();
        assert!(compact::fits_budget(&reply, 7000));
        assert_eq!(reply["result_mode"], "json");
        assert_eq!(reply["output"]["pointer"], "");
        let id = reply["result_id"].as_str().unwrap();
        assert_eq!(store.memory, 0);
        assert!(store.disk <= DISK_BYTES);
        let path = match &store.entries.front().unwrap().stored {
            Stored::File { path, .. } => path.clone(),
            Stored::Memory(_) => panic!("expected spill"),
        };
        let first = store.fetch(&json!({"result_id": id, "mode": "json", "pointer": "", "limit": 37, "max_chars": 1024}), Some("a"), true).unwrap();
        assert!(compact::fits_budget(&first, 1024));
        assert_eq!(first["text"], "{\"output\":\"xxxxxxxxxxxxxxxxxxxxxxxxxx");
        assert_eq!(first["returned"], 37);
        assert!(store.fetch(&json!({"result_id": id, "mode": "json", "pointer": "/output"}), Some("a"), true).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        drop(store);
        assert!(!path.exists());
    }
}
