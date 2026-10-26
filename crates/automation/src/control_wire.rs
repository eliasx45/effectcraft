//! The control channel's wire, shared by the desktop app's control server and the windowless host
//! (`host_server`): JSON-lines requests over loopback TCP, one reply per line. See
//! `docs/control-protocol.md`.
//!
//! The server only ever reads requests: a line that is not a JSON object with a string `method`
//! (an HTTP request line from a browser's cross-origin `fetch`, a stray `nc`, binary junk) gets
//! one error reply and the connection is closed, so nothing after it runs. Lines are bounded
//! ([`MAX_LINE`]) and so is the number of open connections ([`MAX_CONNECTIONS`]).
//!
//! Also here: the `requestId` reply cache (a retry with the same id is answered without running
//! again) and the per-`client` connection count that releases an agent's comp claims when its
//! last connection closes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

/// Longest accepted request line (bytes, without the newline). Requests are small JSON objects;
/// anything longer is refused and the connection closed.
pub const MAX_LINE: usize = 4 * 1024 * 1024;
/// Connections served at once (ten agents, their tools and the user's); further ones get an error
/// line and are closed.
pub const MAX_CONNECTIONS: usize = 32;
/// `requestId`s remembered (with their replies) for retries.
const REPLY_CACHE: usize = 128;
/// Larger replies leave a bounded completion receipt: retrying never reruns their command.
const MAX_CACHED_REPLY: usize = 256 * 1024;
/// Empty or longer identifiers are refused before a request can run.
pub const MAX_REQUEST_ID: usize = 256;
/// Distinct `client` names one connection is tracked for.
const MAX_CLIENTS_PER_CONNECTION: usize = 64;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Accept connections until `stop` is set, running `serve` for each on its own thread (at most
/// [`MAX_CONNECTIONS`] at once). Whoever sets `stop` wakes the loop with a connection of its own.
pub fn accept_loop(listener: TcpListener, stop: Arc<AtomicBool>, serve: impl Fn(TcpStream) + Clone + Send + 'static) {
    let open = Arc::new(AtomicUsize::new(0));
    for mut stream in listener.incoming().flatten() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            open.fetch_sub(1, Ordering::SeqCst);
            let _ = write_line(
                &mut stream,
                &json!({"ok": false, "error": format!("too many control connections (at most {MAX_CONNECTIONS}); closing the connection")}),
            );
            continue;
        }
        let (serve, still_open) = (serve.clone(), Arc::clone(&open));
        let spawned = std::thread::Builder::new().name("ec-control".into()).spawn(move || {
            serve(stream);
            still_open.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Wake an [`accept_loop`] on `port` (after setting its `stop`).
pub fn wake(port: u16) {
    let _ = TcpStream::connect_timeout(&std::net::SocketAddr::from(([127, 0, 0, 1], port)), std::time::Duration::from_secs(1));
}

/// The `client` a request runs as (`engine.execute {command, params: {client}}`).
pub fn request_client<'a>(method: &str, params: &'a Value) -> Option<&'a str> {
    if !matches!(method, "engine.execute" | "ui.menu.invoke") {
        return None;
    }
    params.get("params")?.get("client")?.as_str().filter(|c| !c.is_empty() && c.len() <= effectcraft_engine::agents::MAX_CLIENT_LEN)
}

/// How many open connections run as each `client`.
#[derive(Default)]
pub struct Clients(Mutex<BTreeMap<String, usize>>);

impl Clients {
    /// Remember that a connection (which has used the names in `used`) runs as the request's
    /// `client`.
    pub fn note(&self, used: &mut BTreeSet<String>, method: &str, params: &Value) {
        let Some(c) = request_client(method, params) else { return };
        if used.contains(c) || used.len() >= MAX_CLIENTS_PER_CONNECTION {
            return;
        }
        used.insert(c.to_string());
        let mut clients = lock(&self.0);
        let n = clients.entry(c.to_string()).or_insert(0);
        *n = n.saturating_add(1);
    }

    /// A connection closed: the clients no other open connection uses (release their claims).
    pub fn closed(&self, used: BTreeSet<String>) -> Vec<String> {
        let mut clients = lock(&self.0);
        used.into_iter()
            .filter(|c| match clients.get_mut(c) {
                Some(n) if *n > 1 => {
                    *n -= 1;
                    false
                }
                Some(_) => {
                    clients.remove(c);
                    true
                }
                None => false,
            })
            .collect()
    }
}

/// The last [`REPLY_CACHE`] `requestId`s and their replies (`None` while running).
#[derive(Default)]
pub struct ReplyCache {
    entries: VecDeque<(String, Option<Value>)>,
}

pub enum Lookup {
    Hit(Value),
    Running,
    Miss,
    Rejected(&'static str),
}

impl ReplyCache {
    /// Look `id` up; a new id is recorded as running.
    pub fn begin(&mut self, id: &str) -> Lookup {
        if id.is_empty() || id.len() > MAX_REQUEST_ID {
            return Lookup::Rejected("`requestId` must contain 1 to 256 UTF-8 bytes");
        }
        if let Some((_, r)) = self.entries.iter().find(|(k, _)| k == id) {
            return r.clone().map_or(Lookup::Running, Lookup::Hit);
        }
        if self.entries.len() >= REPLY_CACHE {
            let Some(completed) = self.entries.iter().position(|(_, reply)| reply.is_some()) else {
                return Lookup::Rejected("control reply cache is full of running requests; no command was run; retry later");
            };
            self.entries.remove(completed);
        }
        self.entries.push_back((id.to_string(), None));
        Lookup::Miss
    }

    /// Store `id`'s reply, or forget the id (`None`: it didn't run, so a retry may run it).
    pub fn finish(&mut self, id: &str, reply: Option<Value>) {
        let reply = reply.map(|r| if cacheable(&r) { r } else { completion_receipt(id, &r) });
        let at = self.entries.iter().position(|(k, _)| k == id);
        match (at, reply) {
            (Some(i), Some(r)) => {
                if let Some(e) = self.entries.get_mut(i) {
                    e.1 = Some(r);
                }
            }
            (Some(i), None) => {
                self.entries.remove(i);
            }
            (None, _) => {}
        }
    }
}

/// Count serialized bytes without allocating a second, possibly enormous response.
fn cacheable(reply: &Value) -> bool {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("reply exceeds cache budget"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(MAX_CACHED_REPLY), reply).is_ok()
}

/// This is an unavailable-payload receipt, never a substitute for the original result.
fn completion_receipt(id: &str, reply: &Value) -> Value {
    let original_ok = reply.get("ok").and_then(Value::as_bool);
    let result_ok = reply.pointer("/result/ok").and_then(Value::as_bool);
    let waiting = reply.pointer("/result/waiting").and_then(Value::as_bool);
    let failed = reply.pointer("/result/failed").and_then(Value::as_u64);
    let result_error = reply.pointer("/result/error").filter(|e| !e.is_null());
    let outcome = match original_ok {
        Some(true) if result_ok == Some(false) || failed.is_some_and(|n| n > 0) || result_error.is_some() => "failure",
        Some(true) if waiting == Some(true) => "waiting",
        Some(true) => "success",
        Some(false) => "error",
        None => "unknown",
    };
    let error = reply.get("error").filter(|e| !e.is_null()).or(result_error);
    let error_text = error.and_then(|e| e.as_str().or_else(|| e.get("message").and_then(Value::as_str)));
    let error_detail = error_text.map(|text| text.chars().take(256).collect::<String>());
    json!({
        "ok": false, "replyUnavailable": true, "requestId": id,
        "completed": if waiting == Some(true) { Some(false) } else if original_ok == Some(true) { Some(true) } else { None },
        "outcome": outcome, "originalOk": original_ok, "resultOk": result_ok, "failed": failed, "waiting": waiting,
        "originalError": error_detail,
        "error": format!("request `{id}` already ran and returned an oversized reply (outcome: {outcome}, original ok: {original_ok:?}); the payload is not retained. Do not repeat the mutation; inspect the project or its saved output. This receipt prevents replay only while its requestId remains in this process's cache")
    })
}

/// Answer a request with a `requestId` from `cache`, or run it: `run` returns the reply and
/// whether it ran (a reply for a request that never ran isn't kept, so a retry runs it). `None`:
/// the server is gone.
pub fn once(cache: &Mutex<ReplyCache>, request_id: Option<&str>, run: impl FnOnce() -> Option<(Value, bool)>) -> Option<Value> {
    let Some(rid) = request_id else { return run().map(|(r, _)| r) };
    match lock(cache).begin(rid) {
        Lookup::Hit(reply) => return Some(reply),
        Lookup::Running => {
            return Some(json!({"ok": false, "error": format!("request `{rid}` is still running; retry later with the same requestId to get its reply")}));
        }
        Lookup::Rejected(error) => return Some(json!({"ok": false, "notRun": true, "error": error})),
        Lookup::Miss => {}
    }
    let r = run();
    lock(cache).finish(rid, r.as_ref().and_then(|(reply, ran)| ran.then(|| reply.clone())));
    r.map(|(reply, _)| reply)
}

/// A writer shared by a connection's reply and event threads; each `write` is a whole line, so
/// lines never interleave.
pub struct SharedOut<W>(pub Arc<Mutex<W>>);

impl<W> Clone for SharedOut<W> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<W: Write> Write for SharedOut<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut w = lock(&self.0);
        w.write_all(buf)?;
        w.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        lock(&self.0).flush()
    }
}

/// Write `v` and a newline in one `write_all`.
pub fn write_line(out: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    out.write_all(line.as_bytes())?;
    out.flush()
}

/// One parsed line.
enum Line {
    Request {
        id: Value,
        method: String,
        params: Value,
        /// Top-level `requestId` (string or number): retries with it are answered from the cache.
        request_id: Option<String>,
    },
    Blank,
    /// Not a request: reply with this error and close the connection.
    Reject(String),
}

fn parse_line(bytes: &[u8]) -> Line {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Line::Reject("not a control request (invalid UTF-8); closing the connection".into());
    };
    let text = text.trim();
    if text.is_empty() {
        return Line::Blank;
    }
    let msg = match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(e) => return Line::Reject(format!("bad JSON ({e}); expected one JSON request object per line; closing the connection")),
    };
    let Some(obj) = msg.as_object() else {
        return Line::Reject("not a control request (expected a JSON object); closing the connection".into());
    };
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Line::Reject("not a control request (missing string `method`); closing the connection".into());
    };
    let request_id = match obj.get("requestId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_REQUEST_ID => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(_) => {
            return Line::Reject(format!("`requestId` must be a non-empty string of at most {MAX_REQUEST_ID} UTF-8 bytes or a number; closing the connection"));
        }
    };
    let mut params = obj.get("params").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({}));
    hoist_client(method, obj.get("client"), &mut params);
    Line::Request { id: obj.get("id").cloned().unwrap_or(Value::Null), method: method.to_string(), params, request_id }
}

/// `engine.execute` runs as the `client` in its command's `params`; one written next to `command`
/// (or next to `method`) is moved there, so a script that puts it one level up doesn't silently
/// run as the user (changing his selection and showing up as his edits).
fn hoist_client(method: &str, top: Option<&Value>, params: &mut Value) {
    if !matches!(method, "engine.execute" | "ui.menu.invoke") {
        return;
    }
    let Some(outer) = params.as_object_mut() else { return };
    let Some(client) = outer.remove("client").or_else(|| top.cloned()).filter(Value::is_string) else { return };
    match outer.entry("params").or_insert_with(|| json!({})) {
        Value::Object(inner) => {
            inner.entry("client").or_insert(client);
        }
        p @ Value::Null => *p = json!({"client": client}),
        _ => {}
    }
}

/// Read one `\n`-terminated line of at most `max` bytes into `buf` (newline stripped).
/// `Ok(false)` at end of stream; `Err` when the line is too long or the read fails.
fn read_bounded_line(reader: &mut impl BufRead, buf: &mut Vec<u8>, max: usize) -> Result<bool, String> {
    buf.clear();
    // `take` bounds the bytes buffered for one line: a peer can't make us allocate more than `max`.
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let n = reader.take(limit).read_until(b'\n', buf).map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        return Ok(true);
    }
    if buf.len() > max {
        return Err(format!("request line longer than {max} bytes; closing the connection"));
    }
    // Last line without a newline, then end of stream.
    Ok(true)
}

/// Serve requests from `read`, answering on `out`, until the peer closes, sends something that is
/// not a request, or `handle` returns `None` (server gone). `handle` gets `(method, params,
/// requestId)` and returns the reply object (the request's `id` is added here).
pub fn serve_lines<R: Read, W: Write>(read: R, mut out: W, mut handle: impl FnMut(String, Value, Option<String>) -> Option<Value>) {
    let mut reader = BufReader::new(read);
    let mut buf = Vec::new();
    loop {
        let reject = match read_bounded_line(&mut reader, &mut buf, MAX_LINE) {
            Ok(false) => return,
            Err(e) => e,
            Ok(true) => match parse_line(&buf) {
                Line::Blank => continue,
                Line::Reject(e) => e,
                Line::Request { id, method, params, request_id } => {
                    let Some(mut r) = handle(method, params, request_id) else { return };
                    if let Some(o) = r.as_object_mut() {
                        o.insert("id".into(), id);
                    }
                    if write_line(&mut out, &r).is_err() {
                        return;
                    }
                    continue;
                }
            },
        };
        let _ = write_line(&mut out, &json!({"ok": false, "error": reject}));
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn run(input: &[u8]) -> (Vec<Value>, Vec<String>) {
        let mut out = Vec::new();
        let mut calls = Vec::new();
        serve_lines(input, &mut out, |m, _p, _r| {
            calls.push(m);
            Some(json!({"ok": true, "result": null}))
        });
        let replies = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).collect();
        (replies, calls)
    }

    #[test]
    fn a_client_next_to_command_runs_the_command_as_that_client() {
        let params = |line: &str| match parse_line(line.as_bytes()) {
            Line::Request { params, .. } => params,
            _ => panic!("not a request: {line}"),
        };
        let p = params(r#"{"method":"engine.execute","params":{"command":"script.run","params":{"code":"1"},"client":"ec-comp-diff"}}"#);
        assert_eq!(p, json!({"command": "script.run", "params": {"code": "1", "client": "ec-comp-diff"}}));
        assert_eq!(request_client("engine.execute", &p), Some("ec-comp-diff"));
        // Next to `method`, and with no command params at all.
        let p = params(r#"{"method":"engine.execute","client":"bot","params":{"command":"comp.info"}}"#);
        assert_eq!(p["params"]["client"], "bot");
        // The command's own `client` wins; other methods are left alone.
        let p = params(r#"{"method":"engine.execute","params":{"command":"x","client":"a","params":{"client":"b"}}}"#);
        assert_eq!(p, json!({"command": "x", "params": {"client": "b"}}));
        assert_eq!(params(r#"{"method":"ui.inspect","params":{"client":"a"}}"#), json!({"client": "a"}));
    }

    #[test]
    fn requests_are_answered_in_order_with_ids() {
        let (r, calls) = run(b"{\"id\":1,\"method\":\"a\"}\n\n{\"id\":2,\"method\":\"b\",\"params\":{}}\r\n{\"method\":\"c\"}");
        assert_eq!(calls, ["a", "b", "c"]);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["id"], 1);
        assert_eq!(r[1]["id"], 2);
        assert_eq!(r[2]["id"], Value::Null);
    }

    #[test]
    fn http_request_closes_before_the_body_runs() {
        // A browser's cross-origin `fetch` POST: the request line is rejected and the
        // connection closed, so the JSON body is never executed.
        let http = b"POST / HTTP/1.1\r\nHost: 127.0.0.1:9877\r\nOrigin: https://attacker.example\r\nContent-Type: text/plain\r\n\r\n\
            {\"id\":7,\"method\":\"engine.execute\",\"params\":{\"command\":\"file.new\",\"params\":{}}}\n";
        let (r, calls) = run(http);
        assert!(calls.is_empty(), "nothing may run: {calls:?}");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["ok"], false);
        assert!(r[0]["error"].as_str().unwrap().contains("closing the connection"));
    }

    #[test]
    fn non_request_json_closes_the_connection() {
        for junk in [
            &b"[1,2]\n{\"method\":\"a\"}\n"[..],
            b"{\"id\":1}\n{\"method\":\"a\"}\n",
            b"{\"method\":5}\n{\"method\":\"a\"}\n",
            b"\xff\xfe\n{\"method\":\"a\"}\n",
        ] {
            let (r, calls) = run(junk);
            assert!(calls.is_empty(), "{calls:?}");
            assert_eq!(r.len(), 1);
            assert_eq!(r[0]["ok"], false);
        }
    }

    #[test]
    fn a_request_before_junk_still_runs() {
        let (r, calls) = run(b"{\"method\":\"a\"}\nGET / HTTP/1.1\n{\"method\":\"b\"}\n");
        assert_eq!(calls, ["a"]);
        assert_eq!(r.len(), 2);
        assert_eq!(r[1]["ok"], false);
    }

    #[test]
    fn overlong_line_is_refused() {
        // One byte over the limit, then a valid request: neither runs.
        let mut input = vec![b'x'; MAX_LINE + 1];
        input.extend_from_slice(b"\n{\"method\":\"a\"}\n");
        let (r, calls) = run(&input);
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(r.len(), 1);
        assert!(r[0]["error"].as_str().unwrap().contains("longer than"));
    }

    #[test]
    fn line_at_the_limit_is_accepted() {
        let mut buf = Vec::new();
        let line = vec![b'a'; 10];
        let mut input = line.clone();
        input.push(b'\n');
        assert_eq!(read_bounded_line(&mut &input[..], &mut buf, 10), Ok(true));
        assert_eq!(buf, line);
        assert!(read_bounded_line(&mut &[b'a'; 11][..], &mut buf, 10).is_err());
    }

    #[test]
    fn tcp_connection_is_closed_after_junk() {
        // End to end over a real loopback socket (the transport the app uses).
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let read = s.try_clone().unwrap();
            serve_lines(read, s, |_, _, _| Some(json!({"ok": true})));
        });
        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(b"{\"id\":7,\"method\":\"x\"}\nhello\n{\"method\":\"y\"}\n").unwrap();
        let mut text = String::new();
        c.read_to_string(&mut text).unwrap(); // returns at EOF: the server closed the connection
        server.join().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].contains("\"id\":7"));
        assert!(lines[1].contains("closing the connection"));
    }

    #[test]
    fn retries_keep_oversized_completion_and_release_only_requests_that_did_not_run() {
        let mut cache = ReplyCache::default();
        assert!(matches!(cache.begin("a"), Lookup::Miss));
        assert!(matches!(cache.begin("a"), Lookup::Running));
        cache.finish("a", None);
        assert!(matches!(cache.begin("a"), Lookup::Miss));
        cache.finish("a", Some(json!({"ok": true})));
        assert!(matches!(cache.begin("a"), Lookup::Hit(_)));
        assert!(matches!(cache.begin("big"), Lookup::Miss));
        cache.finish("big", Some(json!({"ok":true,"result":{"png": "x".repeat(MAX_CACHED_REPLY)}})));
        let Lookup::Hit(receipt) = cache.begin("big") else { panic!("completion receipt missing") };
        assert_eq!(receipt["replyUnavailable"], true);
        assert_eq!(receipt["completed"], true);
        assert_eq!(receipt["originalOk"], true);
        assert_eq!(receipt["outcome"], "success");
        assert!(receipt.to_string().len() < 2048);
        let calls = AtomicUsize::new(0);
        let cache = Mutex::new(cache);
        for _ in 0..2 {
            let result = once(&cache, Some("big"), || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some((json!({"ok":true}), true))
            })
            .unwrap();
            assert_eq!(result, receipt);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn oversized_receipts_preserve_script_failure_and_bounded_error_details() {
        let mut cache = ReplyCache::default();
        cache.begin("script");
        cache.finish("script", Some(json!({"ok":true,"result":{"ok":false,"error":{"message":"broken"},"output":"x".repeat(MAX_CACHED_REPLY)}})));
        let Lookup::Hit(receipt) = cache.begin("script") else { panic!("completion receipt missing") };
        assert_eq!(receipt["completed"], true);
        assert_eq!(receipt["outcome"], "failure");
        assert_eq!(receipt["resultOk"], false);
        assert_eq!(receipt["originalError"], "broken");
        cache.begin("error");
        cache.finish("error", Some(json!({"ok":false,"error":"🤖".repeat(MAX_CACHED_REPLY)})));
        let Lookup::Hit(receipt) = cache.begin("error") else { panic!("completion receipt missing") };
        assert_eq!(receipt["originalOk"], false);
        assert_eq!(receipt["completed"], Value::Null, "an error reply need not prove execution completed");
        assert!(receipt.to_string().len() < 2048);
    }

    #[test]
    fn running_entries_are_not_evicted_and_saturation_rejects_before_execution() {
        let cache = Mutex::new(ReplyCache::default());
        for i in 0..REPLY_CACHE {
            assert!(matches!(lock(&cache).begin(&i.to_string()), Lookup::Miss));
        }
        assert!(matches!(lock(&cache).begin("0"), Lookup::Running));
        let mut ran = false;
        let rejection = once(&cache, Some("overflow"), || {
            ran = true;
            Some((json!({"ok":true}), true))
        })
        .unwrap();
        assert!(!ran);
        assert_eq!(rejection["notRun"], true);
        assert!(matches!(lock(&cache).begin("0"), Lookup::Running));
        lock(&cache).finish("1", Some(json!({"ok":true})));
        assert!(matches!(lock(&cache).begin("new"), Lookup::Miss));
        assert!(matches!(lock(&cache).begin("0"), Lookup::Running));
        assert_eq!(lock(&cache).entries.len(), REPLY_CACHE);
    }

    #[test]
    fn finished_entries_can_age_out_and_request_ids_are_bounded() {
        let mut cache = ReplyCache::default();
        for i in 0..REPLY_CACHE {
            cache.begin(&i.to_string());
            cache.finish(&i.to_string(), Some(json!({"ok":true})));
        }
        assert!(matches!(cache.begin("new"), Lookup::Miss));
        assert!(matches!(cache.begin("0"), Lookup::Miss), "finished receipts have finite retention");
        assert!(matches!(cache.begin(""), Lookup::Rejected(_)));
        assert!(matches!(cache.begin(&"x".repeat(MAX_REQUEST_ID + 1)), Lookup::Rejected(_)));
        for id in [json!("x".repeat(MAX_REQUEST_ID + 1)), json!(""), json!({"bad":true})] {
            let input = format!("{}\n", json!({"method":"a","requestId":id}));
            let (replies, calls) = run(input.as_bytes());
            assert!(calls.is_empty());
            assert_eq!(replies[0]["ok"], false);
        }
    }

    #[test]
    fn a_client_is_released_with_its_last_connection() {
        let clients = Clients::default();
        let req = json!({"command": "comp.claim", "params": {"client": "agentA"}});
        let (mut first, mut second) = (BTreeSet::new(), BTreeSet::new());
        clients.note(&mut first, "engine.execute", &req);
        clients.note(&mut second, "engine.execute", &req);
        clients.note(&mut second, "engine.execute", &json!({"params": {}}));
        assert!(clients.closed(first).is_empty(), "still used by the second connection");
        assert_eq!(clients.closed(second), ["agentA"]);
    }
}
