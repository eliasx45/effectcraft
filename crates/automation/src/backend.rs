//! Where tools run: an in-process session, or an app reached through its control channel (the
//! desktop app over TCP, or an offscreen app in this process).

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use effectcraft_engine::Session;
use effectcraft_engine::prefs::Prefs;
use effectcraft_engine::time::Tick;
use serde_json::{Value, json};

use crate::bridge::SessionJob;
use crate::confine::Confinement;
use crate::{BridgeClient, ControlTransport, Error, Result, base64, encode_png, recompress_png};

/// Longest [`Backend::with_session`] waits for an in-process app: its first job waits for the
/// app's first frames, which compile the GPU compositor's shaders (minutes on a cold cache).
const SESSION_WAIT: Duration = Duration::from_secs(600);

pub struct Backend {
    kind: Kind,
    /// Who the agent is (`mcp --client NAME`, or a tool call's `client` argument): every engine
    /// command it runs carries this `client`, so it gets its own selection, active comp and time
    /// and can claim comps (docs/agents.md, "Several agents at once"). `None` = the user's cursor.
    client: Option<String>,
    /// The current tool call named its `client` itself (not the server's default): only then is
    /// a retained result of another client refused ([`Backend::saved_result`]).
    client_named: bool,
    /// File access confined to folders (`--automation-read-root` / `--automation-write-root`).
    confinement: Option<Confinement>,
    expected_revision: Option<u64>,
    results: crate::result_store::ResultStore,
    port: Option<u16>,
}

enum Kind {
    /// An in-process session (no window): engine commands, inspection and rendering.
    Headless(Box<Session>),
    /// An app reached through its control channel (a running desktop app, or the offscreen UI of
    /// `mcp --ui`): everything, plus the UI.
    Bridge(Box<dyn ControlTransport>),
}

/// A rendered comp frame.
pub struct Frame {
    pub png: Vec<u8>,
    pub comp: Value,
    pub time: f64,
    pub width: u32,
    pub height: u32,
    /// Where it rendered with a render box (`render-box` / `app`, [`crate::render_route`]).
    pub rendered_on: Option<String>,
}

impl Backend {
    pub fn headless(session: Session) -> Self {
        Backend {
            kind: Kind::Headless(Box::new(session)),
            client: None,
            client_named: false,
            confinement: None,
            expected_revision: None,
            results: Default::default(),
            port: None,
        }
    }

    /// Bridge to the app's control channel at `addr` (`9877` or `127.0.0.1:9877`).
    pub fn bridge(addr: &str) -> Result<Self> {
        Ok(Backend {
            kind: Kind::Bridge(Box::new(BridgeClient::new(addr)?)),
            client: None,
            client_named: false,
            confinement: None,
            expected_revision: None,
            results: Default::default(),
            port: addr.rsplit(':').next().and_then(|p| p.parse().ok()),
        })
    }

    /// Send renders and lint to a render box at `addr` (`serve --follow`, see
    /// [`crate::render_route`]); everything else still goes to the bridged app. A headless
    /// backend has no app to mirror: refused.
    pub fn with_render_box(mut self, addr: &str) -> Result<Self> {
        let box_client = BridgeClient::new(addr)?;
        self.kind = match self.kind {
            Kind::Bridge(app) => Kind::Bridge(Box::new(crate::render_route::RenderRoute::new(app, Box::new(box_client)))),
            Kind::Headless(_) => return Err(Error::BadArgs("--render-box needs --bridge: the box mirrors a running app".into())),
        };
        Ok(self)
    }

    /// Drive an app through any control transport (e.g. an offscreen UI in this process).
    pub fn transport(t: impl ControlTransport + 'static) -> Self {
        Backend {
            kind: Kind::Bridge(Box::new(t)),
            client: None,
            client_named: false,
            confinement: None,
            expected_revision: None,
            results: Default::default(),
            port: None,
        }
    }

    /// Guard every engine call of a convenience tool. Atomic edits put all writes in one call.
    pub(crate) fn set_expected_revision(&mut self, revision: Option<u64>) -> Option<u64> {
        std::mem::replace(&mut self.expected_revision, revision)
    }

    pub(crate) fn finish_reply(&mut self, name: &str, args: &Value, reply: crate::tools::Reply) -> Result<crate::tools::Reply> {
        let client = match args.get("client").and_then(Value::as_str) {
            Some("") => None,
            Some(client) => Some(client.to_string()),
            None => self.client.clone(),
        };
        match reply {
            crate::tools::Reply::Json(value) => self.results.finish(name, args, value, client.as_deref()).map(crate::tools::Reply::Json),
            crate::tools::Reply::Image { png, info } => Ok(crate::tools::Reply::Image { png, info: self.results.finish(name, args, info, client.as_deref())? }),
        }
    }

    pub(crate) fn saved_result(&self, args: &Value) -> Result<Value> {
        self.results.fetch(args, self.client.as_deref(), self.client_named)
    }

    /// Mark the current tool call as naming its own `client` (see `client_named`). Returns the
    /// previous mark.
    pub(crate) fn set_client_named(&mut self, named: bool) -> bool {
        std::mem::replace(&mut self.client_named, named)
    }

    /// A clearly labelled observation after a write, not a substitute for its own receipt.
    pub(crate) fn observed_context(&mut self) -> Result<Value> {
        let state = self.exec("editor.state", json!({}))?;
        let mut summary = state.clone();
        summary["path"] = state["projectPath"].clone();
        let mut context = crate::host_join::identity(self.port, &json!({"kind": self.mode()}), &summary, self.client());
        context["observedAfter"] = json!(true);
        Ok(context)
    }

    /// Confine the files the commands run here may read and write ([`Confinement`]).
    pub fn confine(&mut self, confinement: Confinement) {
        self.confinement = Some(confinement);
    }

    pub fn confinement(&self) -> Option<&Confinement> {
        self.confinement.as_ref()
    }

    /// The agent's name that engine commands run under (see [`Backend`]'s `client`).
    pub fn client(&self) -> Option<&str> {
        self.client.as_deref()
    }

    /// Run engine commands as `client` (`None`: as the user). Returns the previous client.
    pub fn set_client(&mut self, client: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.client, client.filter(|c| !c.is_empty()))
    }

    /// `params` with this backend's `client` added (unless the call names its own).
    fn with_client(&self, mut params: Value) -> Value {
        if let (Some(c), Some(o)) = (&self.client, params.as_object_mut()) {
            o.entry("client").or_insert_with(|| json!(c));
        }
        params
    }

    /// Whether tools reach an app with a UI (bridge-only tools such as `screenshot` and `ui_*` work).
    pub fn is_bridge(&self) -> bool {
        matches!(self.kind, Kind::Bridge(_))
    }

    /// `headless`, or the transport's mode (`bridge`, `headless-ui`).
    pub fn mode(&self) -> &'static str {
        match &self.kind {
            Kind::Headless(_) => "headless",
            Kind::Bridge(t) => t.mode(),
        }
    }

    pub fn session(&mut self) -> Option<&mut Session> {
        match &mut self.kind {
            Kind::Headless(s) => Some(s),
            Kind::Bridge(_) => None,
        }
    }

    /// The session's preferences when it is in this process: headless, or an app in this process
    /// (`mcp --ui`). `None` for an app in another process.
    pub fn session_prefs(&self) -> Option<Prefs> {
        match &self.kind {
            Kind::Headless(s) => Some(s.prefs.clone()),
            Kind::Bridge(t) => t.session_prefs(),
        }
    }

    /// Run `job` on the session in this process: now (headless), or queued on the UI thread of
    /// an app in this process without waiting ([`ControlTransport::on_session`]). `None` when the
    /// session is in another process.
    pub fn queue_on_session(&mut self, job: SessionJob) -> Option<Result<()>> {
        match &mut self.kind {
            Kind::Headless(s) => {
                job(s);
                Some(Ok(()))
            }
            Kind::Bridge(t) => t.on_session(job),
        }
    }

    /// Run `f` on the session in this process and wait for its result (see
    /// [`Backend::queue_on_session`]). `None` when the session is in another process.
    pub fn with_session<R: Send + 'static>(&mut self, f: impl FnOnce(&mut Session) -> R + Send + 'static) -> Option<Result<R>> {
        if let Kind::Headless(s) = &mut self.kind {
            return Some(Ok(f(s)));
        }
        let (tx, rx) = mpsc::channel();
        if let Err(e) = self.queue_on_session(Box::new(move |s| {
            let _ = tx.send(f(s));
        }))? {
            return Some(Err(e));
        }
        Some(rx.recv_timeout(SESSION_WAIT).map_err(|e| match e {
            RecvTimeoutError::Timeout => Error::Other(format!("the app's session did not answer within {} s", SESSION_WAIT.as_secs())),
            RecvTimeoutError::Disconnected => Error::Other("the app stopped (or failed) before the job on its session finished".into()),
        }))
    }

    /// Run an engine command by id (checked against the [`Confinement`], if any).
    pub fn exec(&mut self, id: &str, params: Value) -> Result<Value> {
        let mut params = self.with_client(if params.is_null() { json!({}) } else { params });
        if let Some(revision) = self.expected_revision {
            if params.get("expectRevision").is_some_and(|v| !v.is_null() && v.as_u64() != Some(revision)) {
                return Err(Error::BadArgs("conflicting expectRevision values; nothing was applied".into()));
            }
            params["expectRevision"] = json!(revision);
        }
        if let Some(c) = self.confinement.clone() {
            let lookup = match c.check_command(id, &params)? {
                Some(crate::confine::Lookup::ProjectFile) => Some((crate::confine::Lookup::ProjectFile, self.run("project.summary", json!({}))?)),
                Some(crate::confine::Lookup::RenderOutputs) => Some((crate::confine::Lookup::RenderOutputs, self.run("renderQueue.list", json!({}))?)),
                None => None,
            };
            if let Some((lookup, found)) = lookup {
                c.check_lookup(&lookup, &found)?;
            }
        }
        self.run(id, params)
    }

    fn run(&mut self, id: &str, params: Value) -> Result<Value> {
        match &mut self.kind {
            Kind::Headless(s) => {
                let r = s.execute_checked(id, params);
                // Toasts (user-facing warnings) stay for the MCP log; other events are dropped.
                s.events.retain(|e| matches!(e, effectcraft_engine::Event::Toast { .. }));
                let excess = s.events.len().saturating_sub(MAX_TOASTS);
                s.events.drain(..excess);
                Ok(r?)
            }
            Kind::Bridge(b) => b.call("engine.execute", json!({"command": id, "params": params})),
        }
    }

    /// Call a raw control-channel method (bridge only).
    pub fn control(&mut self, method: &str, params: Value) -> Result<Value> {
        match &mut self.kind {
            Kind::Bridge(b) => b.call(method, if params.is_null() { json!({}) } else { params }),
            Kind::Headless(_) => Err(Error::BadArgs(NEED_BRIDGE.into())),
        }
    }

    /// `render.frame` with `params` (`comp | item`, `layer`, `time`, `max_side`, `transparent`,
    /// `region`, `overlays`): the engine command headless, the control method bridged (the app
    /// runs the same engine code). Returns the PNG and the reply without it.
    pub fn render_view(&mut self, mut params: Value) -> Result<(Vec<u8>, Value)> {
        let max_side = params.get("max_side").and_then(Value::as_u64).unwrap_or(0).min(u32::MAX as u64) as u32;
        params["base64"] = json!(true);
        // An agent with a `client` renders at its own time and comp: through the engine command,
        // which runs on that client's cursor (the control method uses the user's).
        let mut r = match (&mut self.kind, self.client.is_some()) {
            (Kind::Bridge(b), false) => b.call("render.frame", params)?,
            _ => self.exec("render.frame", params)?,
        };
        let raw = crate::tools::take_png(&mut r, "render.frame")?;
        // (an older app may ignore `max_side`)
        let png = match crate::png_size(&raw) {
            Some((w, h)) if max_side > 0 && w.max(h) > max_side => {
                let (png, w, h) = recompress_png(&raw, max_side)?;
                r["width"] = json!(w);
                r["height"] = json!(h);
                png
            }
            _ => raw,
        };
        Ok((png, r))
    }

    /// Render a comp frame (comp time `time` seconds, default the CTI) as PNG, longest side at most
    /// `max_side` (0 = full size).
    pub fn render(&mut self, comp: Option<&Value>, time: Option<f64>, max_side: u32) -> Result<Frame> {
        self.render_with(comp, time, max_side, false)
    }

    /// [`Backend::render`]; `transparent` keeps the frame's alpha instead of compositing it over
    /// the comp's background colour.
    pub fn render_with(&mut self, comp: Option<&Value>, time: Option<f64>, max_side: u32, transparent: bool) -> Result<Frame> {
        match &mut self.kind {
            Kind::Headless(s) => {
                let cid = s.resolve_comp(comp).map_err(|_| no_comp(comp))?;
                let t = time.map(Tick::from_seconds_f64).unwrap_or_else(|| s.time());
                let (w, h, rgba) = s.render_rgba8_alpha(cid, t, max_side, transparent)?;
                let png = encode_png(w, h, rgba, max_side)?;
                Ok(Frame { png, comp: json!(cid.0), time: t.seconds(), width: w, height: h, rendered_on: None })
            }
            Kind::Bridge(b) => {
                let mut p = json!({"max_side": max_side, "base64": true});
                if transparent {
                    p["transparent"] = json!(true);
                }
                if let Some(t) = time {
                    p["time"] = json!(t);
                }
                if let Some(c) = comp.filter(|c| !c.is_null()) {
                    p["comp"] = c.clone();
                }
                let r = b.call("render.frame", p)?;
                let data = r.get("png").and_then(Value::as_str).ok_or_else(|| Error::App("render.frame returned no `png`".into()))?;
                let raw = base64::decode(data).ok_or_else(|| Error::App("render.frame: bad base64".into()))?;
                let (png, width, height) = recompress_png(&raw, max_side)?;
                let rendered_on = r.get("renderedOn").and_then(Value::as_str).map(str::to_string);
                Ok(Frame { png, comp: r["comp"].clone(), time: r["time"].as_f64().unwrap_or(0.0), width, height, rendered_on })
            }
        }
    }
}

/// Toasts kept between MCP requests at most.
const MAX_TOASTS: usize = 100;

fn no_comp(comp: Option<&Value>) -> Error {
    match comp.filter(|c| !c.is_null()) {
        Some(c) => Error::BadArgs(format!("no composition {c} (see doc_inspect)")),
        None => Error::BadArgs("no active composition: create one (command_run comp.new) or open a project".into()),
    }
}

pub const NEED_BRIDGE: &str = "this tool needs the app's UI: run the MCP server as `effectcraft-cli mcp --ui` (the full app offscreen, no window), or start the app with `effectcraft --control 9877` and run `effectcraft-cli mcp --bridge 9877`";
