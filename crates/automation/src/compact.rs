//! Compact, paged replies for the read tools (`get_comp`, `doc_inspect`, `get_property`,
//! `get_layer`): an agent sees the whole picture of a big comp within a small budget.
//!
//! Clients often cap a tool result (OpenMausBot: 8k characters). The engine's `comp.info` repeats
//! every default switch, blend mode and stretch per layer (~560 characters a layer), so a capped
//! client saw 5 of 110 layers and believed that was all. Rows here keep only what differs from
//! the defaults, and any list that is cut says so in-band (`truncated`), with the offset to
//! continue from. `full: true` keeps the engine's shape.

use serde_json::{Map, Value, json};

use crate::{Error, Result};

/// A requested window into a list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Page {
    pub offset: usize,
    pub limit: usize,
}

/// The largest `limit` honoured (bigger values are clamped).
const MAX_LIMIT: usize = 10_000;

/// A non-negative integer argument (also as a numeric string, which some clients send).
fn count_arg(a: &Value, k: &str) -> Result<Option<usize>> {
    let bad = || Error::BadArgs(format!("`{k}` must be a non-negative integer"));
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(|n| Some(usize::try_from(n).unwrap_or(usize::MAX))).ok_or_else(bad),
        Some(Value::String(s)) => s.trim().parse::<usize>().map(Some).map_err(|_| bad()),
        Some(_) => Err(bad()),
    }
}

impl Page {
    /// `offset` / `limit` from the tool arguments; `limit` defaults to `default_limit`.
    pub fn from_args(a: &Value, default_limit: usize) -> Result<Page> {
        let offset = count_arg(a, "offset")?.unwrap_or(0);
        let limit = count_arg(a, "limit")?.unwrap_or(default_limit);
        if limit == 0 {
            return Err(Error::BadArgs("`limit` must be at least 1".into()));
        }
        Ok(Page { offset, limit: limit.min(MAX_LIMIT) })
    }

    /// Whether the arguments ask for paging explicitly.
    pub fn requested(a: &Value) -> bool {
        ["offset", "limit", "max_chars"].iter().any(|k| a.get(*k).is_some_and(|v| !v.is_null()))
    }
}

/// Conservative serialized UTF-8 budget (the historical argument is named `max_chars`).
pub(crate) const DEFAULT_MAX_CHARS: usize = 7_000;
const MIN_MAX_CHARS: usize = 1_024;
const MAX_MAX_CHARS: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
// Each engine group adds an object and a children array; the tree allows 64 groups.
const MAX_FIELD_DEPTH: usize = 160;

pub(crate) fn budget(a: &Value) -> Result<usize> {
    let n = count_arg(a, "max_chars")?.unwrap_or(DEFAULT_MAX_CHARS);
    if !(MIN_MAX_CHARS..=MAX_MAX_CHARS).contains(&n) {
        return Err(Error::BadArgs(format!("`max_chars` must be between {MIN_MAX_CHARS} and {MAX_MAX_CHARS} UTF-8 bytes")));
    }
    Ok(n)
}

fn full_unbounded(a: &Value) -> bool {
    a.get("full").and_then(Value::as_bool) == Some(true) && !Page::requested(a)
}

/// Count JSON string bytes without allocating an escaped copy of hostile text.
fn string_len(s: &str, cap: usize) -> Option<usize> {
    let mut n = 2usize;
    for ch in s.chars() {
        n = n.checked_add(match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            '\u{00}'..='\u{1f}' => 6,
            _ => ch.len_utf8(),
        })?;
        if n > cap {
            return None;
        }
    }
    (n <= cap).then_some(n)
}

/// Measure exactly like compact serde_json serialization, stopping at the budget or depth cap.
fn json_len(v: &Value, cap: usize, depth: usize) -> Option<usize> {
    json_len_with_depth(v, cap, depth, MAX_DEPTH)
}

fn json_len_with_depth(v: &Value, cap: usize, depth: usize, max_depth: usize) -> Option<usize> {
    if depth > max_depth {
        return None;
    }
    let mut n = match v {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => return string_len(s, cap),
        Value::Array(a) => {
            let mut n = 2usize;
            for (i, child) in a.iter().enumerate() {
                n = n.checked_add(usize::from(i > 0))?;
                n = n.checked_add(json_len_with_depth(child, cap.saturating_sub(n), depth + 1, max_depth)?)?;
                if n > cap {
                    return None;
                }
            }
            n
        }
        Value::Object(o) => {
            let mut n = 2usize;
            for (i, (key, child)) in o.iter().enumerate() {
                n = n.checked_add(usize::from(i > 0))?;
                n = n.checked_add(string_len(key, cap.saturating_sub(n))?)?.checked_add(1)?;
                n = n.checked_add(json_len_with_depth(child, cap.saturating_sub(n), depth + 1, max_depth)?)?;
                if n > cap {
                    return None;
                }
            }
            n
        }
    };
    // Avoid serializing even a scalar when its budget is already exhausted.
    if n > cap {
        n = cap.saturating_add(1);
    }
    (n <= cap).then_some(n)
}

fn child_pointer(pointer: &str, key: &str) -> String {
    format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Pointers always address the original query's JSON, before rows are compacted or paged.
fn omitted(pointer: &str) -> Value {
    json!({"$omitted": true, "editable": false, "pointer": pointer, "fetch": "get_field"})
}

/// Keep exact fields where possible. Arrays are atomic: a partial vector is never an editable value.
fn fit_value(v: Value, pointer: &str, cap: usize, depth: usize) -> Result<Value> {
    if json_len(&v, cap, depth).is_some() {
        return Ok(v);
    }
    let replacement = omitted(pointer);
    let replacement_len =
        json_len(&replacement, cap, 0).ok_or_else(|| Error::BadArgs("`max_chars` cannot fit a field reference; increase the budget".into()))?;
    if depth < MAX_DEPTH
        && let Value::Object(fields) = v
    {
        let mut fixed = 2usize;
        for (i, key) in fields.keys().enumerate() {
            let Some(k) = string_len(key, cap) else {
                return Ok(replacement);
            };
            fixed = fixed.saturating_add(k).saturating_add(1 + usize::from(i > 0));
        }
        let count = fields.len();
        let share = cap.saturating_sub(fixed).checked_div(count).unwrap_or(0);
        if share >= replacement_len {
            let mut out = Map::new();
            for (key, child) in fields {
                let path = child_pointer(pointer, &key);
                let Ok(child) = fit_value(child, &path, share, depth + 1) else {
                    return Ok(replacement);
                };
                out.insert(key, child);
            }
            let out = Value::Object(out);
            if json_len(&out, cap, 0).is_some() {
                return Ok(out);
            }
        }
    }
    Ok(replacement)
}

/// Preserve small source identity fields even when hostile headers collapse to a root reference.
fn fit_header(mut value: Value, cap: usize) -> Result<Value> {
    if json_len(&value, cap, 0).is_some() {
        return Ok(value);
    }
    let mut identity = Map::new();
    if let Some(fields) = value.as_object_mut() {
        for key in
            ["id", "comp", "layer", "revision", "time", "timeBase", "ok", "offset", "total", "returned", "next_offset", "max_chars", "layerCount", "keyCount"]
        {
            if fields.get(key).is_some_and(|v| json_len(v, 128, 0).is_some())
                && let Some(v) = fields.remove(key)
            {
                identity.insert(key.into(), v);
            }
        }
    }
    let reserved = json_len(&Value::Object(identity.clone()), cap, 0).unwrap_or(cap).saturating_add(16);
    let mut out = fit_value(value, "", cap.saturating_sub(reserved), 0)?;
    if out.get("$omitted").and_then(Value::as_bool) == Some(true) && !identity.is_empty() {
        out = json!({"header": out});
    }
    if let Some(fields) = out.as_object_mut() {
        fields.extend(identity);
    }
    if json_len(&out, cap, 0).is_none() {
        return Err(Error::BadArgs("`max_chars` cannot fit query identity and a field reference".into()));
    }
    Ok(out)
}

/// Reusable bound for non-list replies. `full:true` alone preserves the original JSON shape.
pub(crate) fn bounded_reply(r: Value, a: &Value) -> Result<Value> {
    if full_unbounded(a) {
        return Ok(r);
    }
    fit_header(r, budget(a)?)
}

/// Byte-bounded list page. Selected rows use their original indices in fetch pointers.
pub(crate) fn put_page(out: &mut Value, field: &str, items: Vec<Value>, page: Page, count_key: &str, max_chars: usize) -> Result<()> {
    let total = items.len();
    let start = page.offset.min(total);
    let end = start.saturating_add(page.limit).min(total);
    let Some(o) = out.as_object_mut() else {
        return Err(Error::Other("paged query must return a JSON object".into()));
    };
    o.remove(field);
    o.remove("truncated");
    let compact_note = o.remove("compact");
    // Reserve half for rows and continuation metadata, bounding huge headers too.
    *out = fit_header(out.take(), max_chars / 2)?;
    if out.get("$omitted").and_then(Value::as_bool) == Some(true) {
        *out = json!({"header": out.take()});
    }
    out[count_key] = json!(total);
    out["total"] = json!(total);
    out["offset"] = json!(start);
    out["max_chars"] = json!(max_chars);
    if let Some(note) = compact_note {
        out["compact"] = note;
    }
    let mut rows = Vec::new();
    let mut used = 2usize;
    for (index, row) in items.into_iter().enumerate().skip(start).take(end - start) {
        let cap = max_chars.saturating_sub(json_len(out, max_chars, 0).unwrap_or(max_chars)).saturating_sub(300);
        let candidate =
            if rows.is_empty() { fit_value(row, &child_pointer(&child_pointer("", field), &index.to_string()), cap.saturating_sub(3), 0)? } else { row };
        let Some(size) = json_len(&candidate, cap, 0) else {
            break;
        };
        if size.saturating_add(used) > cap {
            break;
        }
        used = used.saturating_add(size).saturating_add(1);
        rows.push(candidate);
    }
    if start < total && rows.is_empty() {
        return Err(Error::BadArgs("`max_chars` cannot fit one row and its continuation metadata; increase the budget".into()));
    }
    let returned = rows.len();
    let next = start.saturating_add(returned);
    let continuation = (next < total).then_some(next);
    out[field] = Value::Array(rows);
    out["returned"] = json!(returned);
    out["next_offset"] = json!(continuation);
    if let Some(next) = continuation {
        out["truncated"] = json!({"field": field, "offset": start, "returned": returned, "total": total, "next_offset": next,
            "hint": format!("call again with offset={next}")});
    }
    if json_len(out, max_chars, 0).is_none() {
        return Err(Error::BadArgs("`max_chars` cannot fit the page metadata; increase the budget".into()));
    }
    Ok(())
}

/// A streaming serializer sink that retains only a requested byte window and boundary context.
struct JsonWindow {
    start: usize,
    end: usize,
    cursor: usize,
    bytes: Vec<u8>,
}

impl JsonWindow {
    fn new(start: usize, end: usize) -> Self {
        Self { start, end, cursor: 0, bytes: Vec::with_capacity(end.saturating_sub(start).min(MAX_LIMIT.saturating_add(6))) }
    }

    fn is_boundary(&self, position: usize, total: usize) -> bool {
        position == total || self.bytes.get(position.saturating_sub(self.start)).is_some_and(|byte| byte & 0xc0 != 0x80)
    }

    fn text(&self, start: usize, end: usize) -> Result<&str> {
        let bytes = self
            .bytes
            .get(start.saturating_sub(self.start)..end.saturating_sub(self.start))
            .ok_or_else(|| Error::Other("invalid field chunk boundary".into()))?;
        std::str::from_utf8(bytes).map_err(|e| Error::Other(format!("invalid UTF-8 in serialized field: {e}")))
    }
}

impl std::io::Write for JsonWindow {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next = self.cursor.checked_add(buffer.len()).ok_or_else(|| std::io::Error::other("serialized field byte count overflow"))?;
        let start = self.start.max(self.cursor);
        let end = self.end.min(next);
        if start < end {
            let bytes = buffer
                .get(start.saturating_sub(self.cursor)..end.saturating_sub(self.cursor))
                .ok_or_else(|| std::io::Error::other("invalid serialized field window"))?;
            if self.bytes.len().saturating_add(bytes.len()) > MAX_LIMIT.saturating_add(6) {
                return Err(std::io::Error::other("serialized field window exceeds its allocation cap"));
            }
            self.bytes.extend_from_slice(bytes);
        }
        self.cursor = next;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Exact JSON-text chunks for `get_field`. Offsets and totals are UTF-8 byte positions.
/// The caller reruns the source query (with a revision guard) before resolving `pointer`.
pub(crate) fn field_chunk(source: &Value, pointer: &str, a: &Value) -> Result<Value> {
    field_chunk_with_budget(source, pointer, a, budget(a)?)
}

/// Internal callers reserve space for their own identity/status metadata.
pub(crate) fn field_chunk_with_budget(source: &Value, pointer: &str, a: &Value, max_chars: usize) -> Result<Value> {
    let page = Page::from_args(a, DEFAULT_MAX_CHARS / 2)?;
    if string_len(pointer, max_chars).is_none() {
        return Err(Error::BadArgs("`max_chars` cannot fit the JSON pointer; fetch an ancestor field instead".into()));
    }
    let field = source.pointer(pointer).ok_or_else(|| Error::BadArgs(format!("no field at JSON pointer `{pointer}`")))?;
    // Count with the engine-tree depth bound before invoking serde's recursive serializer.
    let total = json_len_with_depth(field, usize::MAX, 0, MAX_FIELD_DEPTH)
        .ok_or_else(|| Error::BadArgs("field exceeds the supported JSON depth or byte count".into()))?;
    let start = page.offset.min(total);
    let mut end = start.saturating_add(page.limit).min(total);
    // Three surrounding bytes suffice to detect a split four-byte UTF-8 character.
    let mut window = JsonWindow::new(start.saturating_sub(3), end.saturating_add(3).min(total));
    serde_json::to_writer(&mut window, field).map_err(|e| Error::Other(format!("serialize field: {e}")))?;
    if window.cursor != total {
        return Err(Error::Other("serialized field length changed during measurement".into()));
    }
    if !window.is_boundary(start, total) {
        return Err(Error::BadArgs("`offset` must lie on a UTF-8 boundary".into()));
    }
    while !window.is_boundary(end, total) {
        end = end.saturating_sub(1);
    }
    loop {
        let chunk = window.text(start, end)?;
        let reply = json!({"pointer": pointer, "encoding": "json", "offset": start, "total": total, "returned": end - start,
            "next_offset": (end < total).then_some(end), "text": chunk});
        let mut reply = reply;
        for key in ["comp", "revision"] {
            if let Some(value) = source.get(key) {
                reply[key] = if json_len(value, 128, 0).is_some() { value.clone() } else { omitted(&child_pointer("", key)) };
            }
        }
        if json_len(&reply, max_chars, 0).is_some() && (end > start || start == total) {
            return Ok(reply);
        }
        if end == start {
            return Err(Error::BadArgs("`max_chars` or `limit` cannot fit one UTF-8 character plus field metadata".into()));
        }
        end = end.saturating_sub(1);
        while !window.is_boundary(end, total) {
            end = end.saturating_sub(1);
        }
    }
}

/// A layer switch: its key in `comp.info`'s `switches`, default, and how a differing value reads.
enum Switch {
    /// Shown as `on` when true (default false).
    On(&'static str, &'static str),
    /// Shown as `off` when false (default true).
    Off(&'static str, &'static str),
    /// An option, shown as `label:Value` when not the default.
    Opt(&'static str, &'static str, &'static str),
}

/// Every switch of `effectcraft_project::Switches` (a test checks none is missing).
const SWITCHES: &[Switch] = &[
    Switch::Off("video", "hidden"),
    Switch::Off("audio", "audio_off"),
    Switch::Off("effects", "fx_off"),
    Switch::On("solo", "solo"),
    Switch::On("locked", "locked"),
    Switch::On("shy", "shy"),
    Switch::On("collapse", "collapse"),
    Switch::On("motion_blur", "motion_blur"),
    Switch::On("adjustment", "adjustment"),
    Switch::On("three_d", "3d"),
    Switch::On("guide", "guide"),
    Switch::Opt("quality", "Best", "quality"),
    Switch::Opt("sampling", "Bilinear", "sampling"),
    Switch::Opt("frame_blend", "Off", "frame_blend"),
];

/// The switch keys this module knows.
#[cfg(test)]
pub(crate) fn known_switches() -> Vec<&'static str> {
    SWITCHES
        .iter()
        .map(|s| match s {
            Switch::On(k, _) | Switch::Off(k, _) | Switch::Opt(k, _, _) => *k,
        })
        .collect()
}

/// The switches that differ from the defaults, as short tags (`["3d", "hidden", "quality:Draft"]`).
fn switch_tags(sw: &Value) -> Vec<Value> {
    let Some(m) = sw.as_object() else { return vec![] };
    let mut tags = vec![];
    for s in SWITCHES {
        match s {
            Switch::On(k, tag) if m.get(*k).and_then(Value::as_bool) == Some(true) => tags.push(json!(tag)),
            Switch::Off(k, tag) if m.get(*k).and_then(Value::as_bool) == Some(false) => tags.push(json!(tag)),
            Switch::Opt(k, default, tag) => {
                if let Some(v) = m.get(*k).and_then(Value::as_str).filter(|v| v != default) {
                    tags.push(json!(format!("{tag}:{v}")));
                }
            }
            _ => {}
        }
    }
    // A switch added to the engine later: shown with its value rather than lost.
    let known = |k: &str| SWITCHES.iter().any(|s| matches!(s, Switch::On(n, _) | Switch::Off(n, _) | Switch::Opt(n, _, _) if *n == k));
    for (k, v) in m.iter().filter(|(k, _)| !known(k)) {
        tags.push(json!(format!("{k}:{v}")));
    }
    tags
}

/// One `comp.info` layer as a compact row: index, id, name, type, in, out, then only what is not
/// the default (start ≠ 0, stretch ≠ 100, parent, trackMatte, blend mode ≠ Normal, switches).
pub(crate) fn layer_row(l: &Value) -> Value {
    let mut o = Map::new();
    for k in ["index", "id", "name", "type"] {
        if let Some(v) = l.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    for k in ["in", "out"] {
        if let Some(v) = l.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    if let Some(s) = l.get("start").and_then(Value::as_f64).filter(|s| *s != 0.0) {
        o.insert("start".into(), l.get("start").cloned().unwrap_or_else(|| json!(s)));
    }
    if let Some(s) = l.get("stretch").and_then(Value::as_f64).filter(|s| *s != 100.0) {
        o.insert("stretch".into(), l.get("stretch").cloned().unwrap_or_else(|| json!(s)));
    }
    for k in ["parent", "trackMatte"] {
        if let Some(v) = l.get(k).filter(|v| !v.is_null()) {
            o.insert(k.into(), v.clone());
        }
    }
    if let Some(b) = l.get("blendMode").and_then(Value::as_str).filter(|b| *b != "Normal") {
        o.insert("blendMode".into(), json!(b));
    }
    if let Some(sw) = l.get("switches") {
        let tags = switch_tags(sw);
        if !tags.is_empty() {
            o.insert("switches".into(), Value::Array(tags));
        }
    }
    Value::Object(o)
}

/// Maximum default rows; the byte budget usually selects fewer.
pub(crate) const COMP_LIMIT: usize = 200;
/// The default number of keyframes per `get_property` page.
pub(crate) const KEY_LIMIT: usize = 100;
/// The default number of properties per `get_layer {flat: true}` page.
pub(crate) const FLAT_LIMIT: usize = 150;

/// What compact rows leave out, said once per reply.
const COMP_NOTE: &str = "Compact rows: omitted start = 0, stretch = 100, blendMode = Normal, switches = defaults (tags list the differing ones: hidden = video off, 3d, solo, locked, shy, adjustment, ...). Pass full: true for every field.";

/// A `comp.info` reply shaped for an agent: compact rows unless `full`, paged when asked (and by
/// default when compact). `full` without paging or a byte budget is `comp.info` unchanged.
pub(crate) fn comp_reply(mut info: Value, a: &Value) -> Result<Value> {
    let full = a.get("full").and_then(Value::as_bool) == Some(true);
    if full_unbounded(a) {
        return Ok(info);
    }
    let page = Page::from_args(a, COMP_LIMIT)?;
    let layers = match info.get_mut("layers").map(Value::take) {
        Some(Value::Array(l)) => l,
        _ => vec![],
    };
    let rows = if full { layers } else { layers.iter().map(layer_row).collect() };
    if !full && let Some(o) = info.as_object_mut() {
        o.insert("compact".into(), json!(COMP_NOTE));
    }
    put_page(&mut info, "layers", rows, page, "layerCount", budget(a)?)?;
    Ok(info)
}

/// One `prop.get` keyframe, compact: time (layer s), its comp timecode `tc` and `frame`, value
/// (exact), `compTime` only when it differs from `time`, `in`/`out` only when not Linear and then
/// the `ease` readout (speed and influence per side), `continuous` (false = split handles) and
/// `autoBezier` when set, and any warning.
fn key_row(k: &Value) -> Value {
    let mut o = Map::new();
    if let Some(t) = k.get("time") {
        o.insert("time".into(), t.clone());
    }
    for f in ["tc", "frame"] {
        if let Some(v) = k.get(f) {
            o.insert(f.into(), v.clone());
        }
    }
    if let Some(ct) = k.get("compTime").filter(|ct| k.get("time").is_none_or(|t| t != *ct)) {
        o.insert("compTime".into(), ct.clone());
    }
    if let Some(v) = k.get("value") {
        o.insert("value".into(), v.clone());
    }
    for i in ["in", "out"] {
        if let Some(v) = k.get(i).filter(|v| v.as_str() != Some("Linear")) {
            o.insert(i.into(), v.clone());
        }
    }
    if o.contains_key("in") || o.contains_key("out") {
        for f in ["ease", "continuous"] {
            if let Some(v) = k.get(f) {
                o.insert(f.into(), v.clone());
            }
        }
        if let Some(v) = k.get("autoBezier").filter(|v| v.as_bool() == Some(true)) {
            o.insert("autoBezier".into(), v.clone());
        }
    }
    if let Some(w) = k.get("warning") {
        o.insert("warning".into(), w.clone());
    }
    Value::Object(o)
}

/// A `prop.get` reply with its keyframes compact (unless `full`) and paged: `keyCount`,
/// `returned`, `next_offset`. `full` without paging or a byte budget is `prop.get` unchanged.
pub(crate) fn property_reply(mut r: Value, a: &Value) -> Result<Value> {
    let full = a.get("full").and_then(Value::as_bool) == Some(true);
    if full_unbounded(a) {
        return Ok(r);
    }
    let page = Page::from_args(a, KEY_LIMIT)?;
    let keys = match r.get_mut("keys").map(Value::take) {
        Some(Value::Array(k)) => k,
        _ => vec![],
    };
    let rows = if full { keys } else { keys.iter().map(key_row).collect() };
    put_page(&mut r, "keys", rows, page, "keyCount", budget(a)?)?;
    Ok(r)
}

/// A `get_layer {flat: true}` list, paged: `total`, `returned`, `next_offset`.
pub(crate) fn flat_reply(r: &mut Value, props: Vec<Value>, a: &Value) -> Result<()> {
    if full_unbounded(a) {
        r["properties"] = Value::Array(props);
        return Ok(());
    }
    let page = Page::from_args(a, FLAT_LIMIT)?;
    put_page(r, "properties", props, page, "total", budget(a)?)?;
    Ok(())
}

/// Borrow first, clone only values proven small enough for the reply budget.
pub(crate) fn bounded_from_ref(value: &Value, pointer: &str, cap: usize) -> Result<Value> {
    fn fit(value: &Value, pointer: &str, cap: usize, depth: usize) -> Result<Value> {
        if json_len(value, cap, depth).is_some() {
            return Ok(value.clone());
        }
        let replacement = omitted(pointer);
        let replacement_len = json_len(&replacement, cap, 0).ok_or_else(|| Error::BadArgs("reply budget cannot fit a field reference".into()))?;
        if depth < MAX_DEPTH
            && let Value::Object(fields) = value
        {
            let mut fixed = 2usize;
            for (i, key) in fields.keys().enumerate() {
                let Some(len) = string_len(key, cap) else {
                    return Ok(replacement);
                };
                fixed = fixed.saturating_add(len).saturating_add(1 + usize::from(i > 0));
            }
            let share = cap.saturating_sub(fixed).checked_div(fields.len()).unwrap_or(0);
            if share >= replacement_len {
                let mut out = Map::new();
                for (key, child) in fields {
                    let Ok(child) = fit(child, &child_pointer(pointer, key), share, depth + 1) else {
                        return Ok(replacement);
                    };
                    out.insert(key.clone(), child);
                }
                return Ok(Value::Object(out));
            }
        }
        Ok(replacement)
    }
    fit(value, pointer, cap, 0)
}

/// Bounded depth for retained source JSON; property groups add array/object nesting.
pub(crate) fn source_json_len(value: &Value) -> Result<usize> {
    json_len_with_depth(value, usize::MAX, 0, MAX_FIELD_DEPTH).ok_or_else(|| Error::BadArgs("result exceeds the supported JSON depth or byte count".into()))
}

pub(crate) fn fits_budget(value: &Value, cap: usize) -> bool {
    json_len(value, cap, 0).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_budget(value: &Value, cap: usize) {
        let serialized = serde_json::to_string(value).unwrap();
        assert!(serialized.len() <= cap, "{} > {cap}: {serialized}", serialized.len());
        assert_eq!(json_len(value, cap, 0), Some(serialized.len()));
    }

    #[test]
    fn serialized_measurement_matches_escaped_utf8_and_numbers() {
        for v in [json!("🤖é\n\t\u{0000}\"\\"), json!({"a~/🤖": [null, false, true, 1e-100, 9007199254740993u64]}), json!([]), json!({})] {
            let n = v.to_string().len();
            assert_eq!(json_len(&v, n, 0), Some(n));
            assert!(json_len(&v, n - 1, 0).is_none());
        }
    }

    #[test]
    fn compact_values_and_times_are_exact() {
        let key = json!({"time": 0.123456789123, "compTime": 0.123456789124, "value": [1.234567891234, 9007199254740993u64], "in": "Linear", "out": "Bezier"});
        let row = key_row(&key);
        assert_eq!(row["value"], key["value"]);
        assert_eq!(row["time"], key["time"]);
        assert_eq!(row["compTime"], key["compTime"]);
        let layer = json!({"in": 0.123456789123, "out": 1.234567891234, "start": 0.0000000001, "stretch": 100.0000000001});
        let row = layer_row(&layer);
        for key in ["in", "out", "start", "stretch"] {
            assert_eq!(row[key], layer[key]);
        }
    }

    #[test]
    fn budget_pages_advance_without_losing_rows() {
        let layers: Vec<_> = (0..300).map(|i| json!({"id": i, "name": "🤖".repeat(20), "in": 0.0, "out": 1.23456789123})).collect();
        let source = json!({"comp": 9, "revision": 12, "layers": layers});
        let mut offset = 0;
        let mut ids = Vec::new();
        loop {
            let result = comp_reply(source.clone(), &json!({"offset": offset})).unwrap();
            assert_budget(&result, DEFAULT_MAX_CHARS);
            assert_eq!(result["total"], 300);
            assert_eq!(result["layerCount"], 300);
            assert_eq!(result["offset"], offset);
            assert_eq!(result["comp"], 9);
            let rows = result["layers"].as_array().unwrap();
            assert!(!rows.is_empty());
            ids.extend(rows.iter().map(|r| r["id"].as_u64().unwrap()));
            let Some(next) = result["next_offset"].as_u64() else {
                break;
            };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert_eq!(ids, (0..300).collect::<Vec<u64>>());
    }

    #[test]
    fn oversized_rows_use_original_indices_and_exact_fetch_pointers() {
        let source = json!({"comp": 1, "revision": 7, "keys": [
            {"time": 0, "value": "small"},
            {"time": 1.0000000001, "value": "🤖\n".repeat(10_000)}
        ]});
        let result = property_reply(source.clone(), &json!({"offset": 1, "max_chars": 1024})).unwrap();
        assert_budget(&result, 1024);
        assert_eq!(result["returned"], 1);
        assert_eq!(result["next_offset"], Value::Null);
        let row = &result["keys"][0];
        assert_eq!(row["time"], source["keys"][1]["time"]);
        assert_eq!(row["value"]["pointer"], "/keys/1/value");
        assert_eq!(row["value"]["editable"], false);
        assert_eq!(row["value"]["fetch"], "get_field");
        assert_eq!(source.pointer(row["value"]["pointer"].as_str().unwrap()), Some(&source["keys"][1]["value"]));
    }

    #[test]
    fn enormous_header_text_and_keys_remain_fetchable_and_bounded() {
        for key in ["name".to_owned(), "🤖/key~".repeat(2000)] {
            let mut source = json!({"comp": 3, "revision": 99, "time": 0.123456789123, "layers": [{"id": 1}]});
            source[key] = json!("🤖\n".repeat(10_000));
            let result = comp_reply(source.clone(), &json!({"max_chars": 1024})).unwrap();
            assert_budget(&result, 1024);
            assert_eq!(result["comp"], 3);
            assert_eq!(result["revision"], 99);
            assert_eq!(result["time"], source["time"]);
            assert_eq!(result["returned"], 1);
            assert_eq!(result["total"], 1);
        }
    }

    #[test]
    fn tree_and_arrays_have_explicit_atomic_placeholders() {
        let source = json!({"comp": 2, "revision": 5, "a~/b": "x".repeat(10_000), "value": vec![1.23456789123; 10_000]});
        let result = bounded_reply(source.clone(), &json!({})).unwrap();
        assert_budget(&result, DEFAULT_MAX_CHARS);
        assert_eq!(result["a~/b"]["pointer"], "/a~0~1b");
        assert_eq!(result["value"]["pointer"], "/value");
        assert_eq!(result["value"]["editable"], false);
        assert_eq!(result["revision"], 5);
        assert_eq!(source.pointer("/a~0~1b"), source.get("a~/b"));
    }

    #[test]
    fn field_chunks_reassemble_exact_json_with_unicode_and_escape_sequences() {
        let source = json!({"comp": 1, "revision": 9, "text": "🤖\n\\\"é".repeat(1000)});
        let expected = source["text"].to_string();
        let mut offset = 0usize;
        let mut restored = String::new();
        loop {
            let chunk = field_chunk(&source, "/text", &json!({"offset": offset, "limit": 71, "max_chars": 1024})).unwrap();
            assert_budget(&chunk, 1024);
            assert_eq!(chunk["revision"], 9);
            assert_eq!(chunk["comp"], 1);
            assert_eq!(chunk["total"], expected.len());
            restored.push_str(chunk["text"].as_str().unwrap());
            let Some(next) = chunk["next_offset"].as_u64() else {
                break;
            };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert_eq!(restored, expected);
        assert_eq!(serde_json::from_str::<Value>(&restored).unwrap(), source["text"]);
        assert!(field_chunk(&source, "/text", &json!({"offset": 2})).is_err());
        assert!(field_chunk(&source, "/text", &json!({"offset": 1, "limit": 1})).is_err());
    }

    #[test]
    fn full_escape_hatch_only_applies_without_explicit_budget_or_paging() {
        let source = json!({"name": "x".repeat(20_000), "layers": [{"id": 1}]});
        assert_eq!(comp_reply(source.clone(), &json!({"full": true})).unwrap(), source);
        assert_budget(&comp_reply(source.clone(), &json!({"full": true, "max_chars": 7000})).unwrap(), 7000);
        assert_eq!(bounded_reply(source.clone(), &json!({"full": true})).unwrap(), source);
        for bad in [json!({"max_chars": 0}), json!({"max_chars": 100}), json!({"max_chars": 1_000_001}), json!({"max_chars": -1}), json!({"limit": 0})] {
            assert!(comp_reply(source.clone(), &bad).is_err());
        }
    }

    #[test]
    fn empty_and_out_of_range_pages_terminate() {
        for source in [json!({"layers": []}), json!({"layers": [{"id": 1}]})] {
            let result = comp_reply(source, &json!({"offset": usize::MAX})).unwrap();
            assert_budget(&result, DEFAULT_MAX_CHARS);
            assert_eq!(result["returned"], 0);
            assert_eq!(result["next_offset"], Value::Null);
        }
    }

    #[test]
    fn streaming_field_serialization_retains_only_the_requested_window() {
        let field = json!("🤖\n".repeat(200_000));
        let total = json_len_with_depth(&field, usize::MAX, 0, MAX_FIELD_DEPTH).unwrap();
        let mut window = JsonWindow::new(1000, 1046);
        serde_json::to_writer(&mut window, &field).unwrap();
        assert_eq!(window.cursor, total);
        assert_eq!(window.bytes.len(), 46);
        assert!(window.bytes.capacity() <= MAX_LIMIT + 6);
        let source = json!({"field": field});
        let result = field_chunk(&source, "/field", &json!({"offset": 1, "limit": 41, "max_chars": 1024})).unwrap();
        assert_budget(&result, 1024);
        assert!(result["returned"].as_u64().unwrap() <= 41);
        assert_eq!(result["total"], total);
    }

    #[test]
    fn field_chunks_recover_forty_nested_property_groups() {
        let mut tree = json!({"name": "Leaf", "value": 1.234567891234});
        for i in 0..40 {
            tree = json!({"name": format!("Group {i}"), "children": [tree]});
        }
        let source = json!({"comp": 2, "revision": 5, "properties": tree});
        assert!(json_len(&source, usize::MAX, 0).is_none(), "tree exceeds the compact traversal depth");
        let bounded = bounded_reply(source.clone(), &json!({})).unwrap();
        assert_budget(&bounded, DEFAULT_MAX_CHARS);
        let expected = serde_json::to_string(&source).unwrap();
        let mut restored = String::new();
        let mut offset = 0usize;
        loop {
            let reply = field_chunk(&source, "", &json!({"offset": offset, "limit": 127})).unwrap();
            assert_budget(&reply, DEFAULT_MAX_CHARS);
            restored.push_str(reply["text"].as_str().unwrap());
            let Some(next) = reply["next_offset"].as_u64() else {
                break;
            };
            assert!(next as usize > offset);
            offset = next as usize;
        }
        assert_eq!(restored, expected);
        assert_eq!(serde_json::from_str::<Value>(&restored).unwrap(), source);
    }

    #[test]
    fn hostile_depth_is_bounded_and_field_fetch_reports_limit() {
        let mut value = json!(1);
        for _ in 0..200 {
            value = json!({"child": value});
        }
        let result = bounded_reply(value.clone(), &json!({})).unwrap();
        assert_budget(&result, DEFAULT_MAX_CHARS);
        assert!(field_chunk(&value, "", &json!({})).is_err());
    }
}
