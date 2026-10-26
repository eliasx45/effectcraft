//! Blocking client for the desktop app's JSON-lines control channel (`docs/control-protocol.md`).
//! One persistent loopback connection; uncertain writes are replayed only to the same host instance.
//!
//! [`ControlTransport`] abstracts where control requests go: [`BridgeClient`] (a running app over
//! TCP) or an app embedded in this process, such as `effectcraft-cli mcp --ui`'s offscreen UI
//! (implemented above this crate, which never depends on egui).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use effectcraft_engine::Session;
use effectcraft_engine::prefs::Prefs;
use serde_json::{Value, json};

use crate::{Error, Result};

/// Code run on the session of an app in this process ([`ControlTransport::on_session`]).
pub type SessionJob = Box<dyn FnOnce(&mut Session) + Send>;

/// Slightly longer than the app's own 60 s per-request timeout, so its error reply wins.
const READ_TIMEOUT: Duration = Duration::from_secs(65);

/// Something that answers control-channel requests (`docs/control-protocol.md`).
pub trait ControlTransport: Send {
    /// Call a control method; returns the reply's `result`, or the app's `error` as [`Error::App`].
    fn call(&mut self, method: &str, params: Value) -> Result<Value>;
    /// The MCP mode name this transport gives (`initialize`'s server title, `effectcraft-cli info`).
    fn mode(&self) -> &'static str {
        "bridge"
    }
    /// The session's preferences when the app runs in this process (`None` for an app in another
    /// process, whose session is out of reach).
    fn session_prefs(&self) -> Option<Prefs> {
        None
    }
    /// Queue `job` on the app's session when the app runs in this process: it runs on the app's
    /// UI thread, between frames, before any later request, and this doesn't wait for it. `None`
    /// when the app is in another process; `Err` when the app stopped.
    fn on_session(&mut self, _job: SessionJob) -> Option<Result<()>> {
        None
    }
}

/// Unwrap a control reply (`{"ok": true, "result": …}` / `{"ok": false, "error": …}`).
pub fn reply_result(v: Value) -> Result<Value> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    Err(Error::App(v.get("error").and_then(Value::as_str).unwrap_or("error").to_string()))
}

impl ControlTransport for BridgeClient {
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        BridgeClient::call(self, method, params)
    }
}

pub struct BridgeClient {
    addr: String,
    conn: Option<Connection>,
    next_id: u64,
    request_prefix: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HostInstance {
    process: String,
    session: Option<String>,
}

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    instance: Option<HostInstance>,
}

fn host_instance(capabilities: &Value) -> Option<HostInstance> {
    let identity = |key: &str| capabilities.get(key).and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 256).map(str::to_owned);
    Some(HostInstance { process: identity("processInstance")?, session: identity("sessionId") })
}

/// Older hosts without an instance id may still answer a fresh call. Only these harmless
/// reads can be resent there after a lost reply; unknown commands default to no replay.
fn harmless_read(method: &str, params: &Value) -> bool {
    match method {
        "engine.commands" | "ui.menu.list" | "ui.inspect" | "ui.renderTimes" | "ui.elements" | "ui.viewer.locate" | "ui.viewer.hit" | "ui.timeline.locate"
        | "app.revision" => true,
        "ui.playback" => params["action"] == "status",
        "engine.execute" => params.get("command").and_then(Value::as_str).is_some_and(|command| {
            matches!(
                command,
                "project.summary"
                    | "comp.info"
                    | "layer.tree"
                    | "prop.get"
                    | "editor.state"
                    | "app.capabilities"
                    | "command.list"
                    | "command.describe"
                    | "jobs.list"
            )
        }),
        _ => false,
    }
}

/// Transport loss may leave execution uncertain, including a partially successful write.
/// Timeouts and malformed replies are not replayed automatically.
fn exchange(conn: &mut Connection, id: u64, request_id: Option<&str>, method: &str, params: &Value) -> Result<std::result::Result<Value, Error>> {
    let mut request = json!({"id": id, "method": method, "params": params});
    if let Some(rid) = request_id {
        request["requestId"] = json!(rid);
    }
    let line = format!("{request}\n");
    if let Err(error) = conn.writer.write_all(line.as_bytes()) {
        return Ok(Err(Error::Bridge(error.to_string())));
    }
    let mut buf = String::new();
    match conn.reader.read_line(&mut buf) {
        Ok(0) => return Ok(Err(Error::Bridge("connection closed by the app".into()))),
        Ok(_) => {}
        Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            return Err(Error::Bridge(format!(
                "no reply to `{method}` within {}s; execution may have completed (inspect the project before retrying)",
                READ_TIMEOUT.as_secs()
            )));
        }
        Err(error) => return Ok(Err(Error::Bridge(error.to_string()))),
    }
    let reply: Value = serde_json::from_str(&buf).map_err(|error| Error::Bridge(format!("bad reply: {error}; execution may have completed")))?;
    if reply.get("id") != Some(&json!(id)) {
        return Err(Error::Bridge("reply id does not match the request; execution may have completed".into()));
    }
    Ok(Ok(reply))
}

impl BridgeClient {
    /// `addr`: a port (`9877`) or `host:port` on loopback (`127.0.0.1`, `localhost`, `[::1]`).
    pub fn new(addr: &str) -> Result<Self> {
        let addr = if addr.parse::<u16>().is_ok() { format!("127.0.0.1:{addr}") } else { addr.to_string() };
        let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(&addr);
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
            return Err(Error::BadArgs(format!("bridge address must be loopback (PORT or 127.0.0.1:PORT), got `{addr}`")));
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|e| Error::Bridge(format!("cannot create a unique control request identity: {e}")))?;
        let request_prefix = format!("mcp-{:032x}", u128::from_be_bytes(random));
        Ok(Self { addr, conn: None, next_id: 1, request_prefix })
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn connect(&self) -> Result<Connection> {
        let sa = self
            .addr
            .to_socket_addrs()
            .map_err(|e| Error::Bridge(format!("{}: {e}", self.addr)))?
            .next()
            .ok_or_else(|| Error::Bridge(format!("cannot resolve {}", self.addr)))?;
        let s = TcpStream::connect_timeout(&sa, Duration::from_secs(5))
            .map_err(|e| {
                Error::Bridge(format!(
                    "cannot connect to {} ({e}); start the app with `effectcraft --control <port>`, or find where the project is open now with the MCP `hosts` tool (`hosts {{join: <project>}}`)",
                    self.addr
                ))
            })?;
        s.set_read_timeout(Some(READ_TIMEOUT)).ok();
        s.set_nodelay(true).ok();
        let r = s.try_clone().map_err(|e| Error::Bridge(e.to_string()))?;
        let mut conn = Connection { reader: BufReader::new(r), writer: s, instance: None };
        let reply = match exchange(&mut conn, 0, None, "engine.execute", &json!({"command":"app.capabilities","params":{}})) {
            Ok(Ok(reply)) => reply,
            Ok(Err(error)) | Err(error) => return Err(error),
        };
        // Unsupported capabilities on an older host leave identity unknown, but do not
        // prevent ordinary calls. Unknown identity cannot authorize replaying a write.
        conn.instance = reply_result(reply).ok().and_then(|capabilities| host_instance(&capabilities));
        Ok(conn)
    }

    /// Call a control method; returns the reply's `result`, or the app's `error` as [`Error::App`].
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let unknown = |error| Error::Bridge(format!("`{method}` request outcome is unknown: {error}; inspect the app before retrying"));
        let mut last = Error::Bridge("not attempted".into());
        let request_id = format!("{}-{}", self.request_prefix, self.next_id);
        let mut first_instance = None;
        for attempt in 0..2 {
            if self.conn.is_none() {
                self.conn = Some(self.connect().map_err(|error| {
                    if attempt == 0 {
                        error
                    } else {
                        Error::Bridge(format!("request `{request_id}` may have completed; reconnect failed before any resend: {error}"))
                    }
                })?);
            }
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).ok_or_else(|| Error::Bridge("control request ids exhausted; create a new bridge".into()))?;
            let Some(conn) = self.conn.as_mut() else { continue };
            if attempt == 0 {
                first_instance = conn.instance.clone();
            } else {
                let same = first_instance.as_ref().zip(conn.instance.as_ref()).is_some_and(|(first, current)| first == current);
                let changed = first_instance.as_ref().zip(conn.instance.as_ref()).is_some_and(|(first, current)| first != current);
                if !same && (changed || !harmless_read(method, &params)) {
                    return Err(Error::Bridge(format!(
                        "cannot safely replay request `{request_id}`: the outcome is unknown because the host process/session changed or its identity is unknown. The first attempt may have completed; no request was resent. Inspect the project or saved output before making a new mutation"
                    )));
                }
            }
            match exchange(conn, id, Some(&request_id), method, &params) {
                Ok(Ok(reply)) => return reply_result(reply),
                Ok(Err(error)) => {
                    self.conn = None;
                    last = error;
                }
                Err(error) => {
                    self.conn = None;
                    return Err(if harmless_read(method, &params) { error } else { unknown(error) });
                }
            }
        }
        Err(Error::Bridge(format!("request `{request_id}` may have completed; no reply after reconnect: {last}")))
    }
}

#[cfg(test)]
mod retry_tests {
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::control_wire::{self, ReplyCache};

    #[test]
    fn dropped_oversized_mutation_reply_reconnects_to_a_receipt_without_reexecution() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let cache = Mutex::new(ReplyCache::default());
            let mut session = Session::default();
            let mutations = AtomicUsize::new(0);
            let mut retried_receipt = None;
            for connection in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                control_wire::serve_lines(stream.try_clone().unwrap(), stream, |method, params, rid| {
                    let is_mutation = params["command"] == "comp.new";
                    let reply = control_wire::once(&cache, rid.as_deref(), || {
                        assert_eq!(method, "engine.execute");
                        let command = params["command"].as_str().unwrap();
                        let result = if command == "app.capabilities" {
                            json!({"processInstance":"same-process","sessionId":"same-session"})
                        } else {
                            let mut result = session.execute(command, params["params"].clone()).unwrap();
                            if is_mutation {
                                mutations.fetch_add(1, Ordering::SeqCst);
                                result["largeOutput"] = json!("x".repeat(300_000));
                            }
                            result
                        };
                        Some((json!({"ok":true,"result":result}), true))
                    });
                    if is_mutation && connection == 0 {
                        return None; // Lost TCP reply after the mutation and cache finish.
                    }
                    if is_mutation {
                        retried_receipt = reply.clone();
                    }
                    reply
                });
            }
            (mutations.load(Ordering::SeqCst), retried_receipt.unwrap())
        });
        let mut bridge = BridgeClient::new(&addr.to_string()).unwrap();
        let error = bridge.call("engine.execute", json!({"command":"comp.new","params":{"name":"Once"}})).unwrap_err();
        assert!(error.to_string().contains("already ran"), "{error}");
        assert!(error.to_string().contains("outcome: success"), "{error}");
        let project = bridge.call("engine.execute", json!({"command":"project.summary","params":{}})).unwrap();
        assert_eq!(project["items"].as_array().unwrap().len(), 1, "{project}");
        drop(bridge);
        let (mutations, receipt) = worker.join().unwrap();
        assert_eq!(mutations, 1);
        assert_eq!(receipt["completed"], true);
        assert_eq!(receipt["replyUnavailable"], true);
    }

    fn reconnect(first: Value, second: Value, command: &str) -> (Result<Value>, usize) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            let attempts = AtomicUsize::new(0);
            for (index, identity) in [first, second].into_iter().enumerate() {
                let (stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                control_wire::serve_lines(stream.try_clone().unwrap(), stream, |_, params, _| {
                    if params["command"] == "app.capabilities" {
                        return Some(json!({"ok":true,"result":identity}));
                    }
                    attempts.fetch_add(1, Ordering::SeqCst);
                    if index == 0 { None } else { Some(json!({"ok":true,"result":{"read":"legacy"}})) }
                });
            }
            attempts.load(Ordering::SeqCst)
        });
        let mut bridge = BridgeClient::new(&addr.to_string()).unwrap();
        let result = bridge.call("engine.execute", json!({"command":command,"params":{}}));
        drop(bridge);
        (result, worker.join().unwrap())
    }

    #[test]
    fn uncertain_requests_are_not_replayed_to_a_different_process_or_session() {
        let first = json!({"processInstance":"process-a","sessionId":"session-a"});
        for second in [json!({"processInstance":"process-b","sessionId":"session-a"}), json!({"processInstance":"process-a","sessionId":"session-b"})] {
            for command in ["comp.new", "project.summary"] {
                let (result, attempts) = reconnect(first.clone(), second.clone(), command);
                let error = result.unwrap_err().to_string();
                assert!(error.contains("cannot safely replay"), "{error}");
                assert!(error.contains("may have completed"), "{error}");
                assert_eq!(attempts, 1);
            }
        }
    }

    #[test]
    fn older_hosts_allow_fresh_writes_but_only_harmless_read_replays() {
        let (result, attempts) = reconnect(json!({"version":"old"}), json!({"version":"old"}), "project.summary");
        assert_eq!(result.unwrap()["read"], "legacy");
        assert_eq!(attempts, 2);
        let (result, attempts) = reconnect(json!({}), json!({}), "comp.new");
        assert!(result.unwrap_err().to_string().contains("identity is unknown"));
        assert_eq!(attempts, 1);
        assert!(!harmless_read("engine.execute", &json!({"command":"new.unknown"})));
        assert!(!harmless_read("render.frame", &json!({})));
    }

    #[test]
    fn two_bridge_clients_have_distinct_request_namespaces() {
        let first = BridgeClient::new("9877").unwrap();
        let second = BridgeClient::new("9877").unwrap();
        assert_ne!(first.request_prefix, second.request_prefix);
        assert!(first.request_prefix.len() + 1 + u64::MAX.to_string().len() <= control_wire::MAX_REQUEST_ID);
    }
}
