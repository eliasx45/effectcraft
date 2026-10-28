//! Several agents at once: each `client` gets its own cursor, and a client can claim a
//! composition so other clients leave it alone.
//!
//! **Cursors.** Selection, active comp and current time live in [`EditorState`]. A command run
//! with a `client` execute parameter works on that client's own [`Cursor`]: [`Session::execute`]
//! swaps it into `state` for the command and swaps the user's back afterwards, so an agent
//! selecting layers, moving the time or creating (opening) a comp never moves the user's view.
//! The first command of a client starts from a copy of the user's cursor. At most
//! [`MAX_CURSORS`] are kept (the least recently used goes first).
//!
//! **Claims.** `comp.claim {comp, ttl?}` reserves a comp for a client for `ttl` seconds
//! ([`DEFAULT_TTL`], at most [`MAX_TTL`]); the owner renews it by claiming again. While it holds,
//! an edit by *another* client that changes the comp (its layers, properties, name) or deletes it
//! is refused, changing nothing ([`Session::edit`], and a catch-all check after each client
//! command for changes made outside `edit`, e.g. undo). The user (no `client`) is never blocked
//! and needs no claim. Claims expire on their own (dropped lazily), and the control channel
//! releases a connection's claims when it closes.

use std::collections::BTreeMap;
use std::time::Duration;

use effectcraft_project::{ItemId, LayerId, Project, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, b_p, bad, comp_id, f_p};
use crate::{CompSelection, EditorState, EngineError, KeyRef, Result, Session, VertexRef, query};

/// Client cursors kept at most (the least recently used is dropped).
pub const MAX_CURSORS: usize = 64;
/// Longest accepted `client` name (bytes).
pub const MAX_CLIENT_LEN: usize = 128;
/// A claim's lifetime when `ttl` isn't given (seconds).
pub const DEFAULT_TTL: f64 = 300.0;
/// The longest claim (seconds): 24 hours.
pub const MAX_TTL: f64 = 86_400.0;

// ---------------------------------------------------------------- cursors

/// The parts of [`EditorState`] that belong to whoever is working: active comp, current times,
/// selections, the Tracker panel's current track and on-canvas text editing. Everything else
/// (open comp tabs, clipboards, tool and viewer options) is shared.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub active_comp: Option<ItemId>,
    pub recent_comps: Vec<ItemId>,
    pub comp_selections: BTreeMap<ItemId, CompSelection>,
    pub times: BTreeMap<ItemId, Tick>,
    pub selected_layers: Vec<LayerId>,
    pub selected_props: Vec<(LayerId, Uid)>,
    pub selected_keys: Vec<KeyRef>,
    pub selected_vertices: Vec<VertexRef>,
    pub project_selection: Vec<ItemId>,
    pub current_track: Option<(LayerId, Uid)>,
    pub camera_points: Vec<u32>,
    pub text_edit: Option<crate::commands::text_edit::TextEdit>,
}

impl Cursor {
    /// A copy of `st`'s cursor (a new client starts where the user is), without text editing.
    pub fn of(st: &EditorState) -> Cursor {
        Cursor {
            active_comp: st.active_comp,
            recent_comps: st.recent_comps.clone(),
            comp_selections: st.comp_selections.clone(),
            times: st.times.clone(),
            selected_layers: st.selected_layers.clone(),
            selected_props: st.selected_props.clone(),
            selected_keys: st.selected_keys.clone(),
            selected_vertices: st.selected_vertices.clone(),
            project_selection: st.project_selection.clone(),
            current_track: st.current_track,
            camera_points: st.camera_points.clone(),
            text_edit: None,
        }
    }

    /// Exchange this cursor with `st`'s.
    pub fn swap(&mut self, st: &mut EditorState) {
        use std::mem::swap;
        swap(&mut self.active_comp, &mut st.active_comp);
        swap(&mut self.recent_comps, &mut st.recent_comps);
        swap(&mut self.comp_selections, &mut st.comp_selections);
        swap(&mut self.times, &mut st.times);
        swap(&mut self.selected_layers, &mut st.selected_layers);
        swap(&mut self.selected_props, &mut st.selected_props);
        swap(&mut self.selected_keys, &mut st.selected_keys);
        swap(&mut self.selected_vertices, &mut st.selected_vertices);
        swap(&mut self.project_selection, &mut st.project_selection);
        swap(&mut self.current_track, &mut st.current_track);
        swap(&mut self.camera_points, &mut st.camera_points);
        swap(&mut self.text_edit, &mut st.text_edit);
    }
}

/// The clients' cursors (not the user's: that one is in [`Session::state`]).
#[derive(Clone, Debug, Default)]
pub struct Cursors {
    /// Client → (cursor, last use).
    map: BTreeMap<String, (Cursor, u64)>,
    clock: u64,
    /// The cursors of whoever ran before while a client's command runs (innermost last): the
    /// first is the user's, which is what a project file saves ([`Session::open_comps_to_save`]).
    suspended: Vec<Cursor>,
}

impl Cursors {
    /// Remove and return a client's cursor.
    pub fn take(&mut self, client: &str) -> Option<Cursor> {
        self.map.remove(client).map(|(c, _)| c)
    }

    /// Store a client's cursor, dropping the least recently used beyond [`MAX_CURSORS`].
    pub fn put(&mut self, client: String, cursor: Cursor) {
        self.clock = self.clock.saturating_add(1);
        self.map.insert(client, (cursor, self.clock));
        while self.map.len() > MAX_CURSORS {
            let Some(oldest) = self.map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone()) else { break };
            self.map.remove(&oldest);
        }
    }

    pub fn get(&self, client: &str) -> Option<&Cursor> {
        self.map.get(client).map(|(c, _)| c)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Forget every client cursor (another project was opened). The suspended outer cursors
    /// point into the old project too: [`run_as`] starts them again from the current state.
    pub fn clear(&mut self) {
        self.map.clear();
        self.suspended.clear();
    }

    /// The user's cursor while a client's command runs (`None`: no client command runs, the
    /// user's cursor is [`Session::state`]'s).
    pub fn user(&self) -> Option<&Cursor> {
        self.suspended.first()
    }

    /// Clients, most recently used first.
    pub fn clients(&self) -> Vec<(&str, &Cursor)> {
        let mut v: Vec<(&str, &Cursor, u64)> = self.map.iter().map(|(k, (c, t))| (k.as_str(), c, *t)).collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.2));
        v.into_iter().map(|(k, c, _)| (k, c)).collect()
    }
}

/// Run `f` (a command) as `client`: its cursor swapped in, the user's (or the calling client's)
/// swapped back afterwards, also on error. Edits from it that change a composition another
/// client claimed are undone and reported.
pub(crate) fn run_as(s: &mut Session, client: String, f: impl FnOnce(&mut Session) -> Result<Value>) -> Result<Value> {
    if client.len() > MAX_CLIENT_LEN {
        return Err(EngineError::Other(format!("`client` is longer than {MAX_CLIENT_LEN} bytes")));
    }
    // `outer` holds the cursor of whoever ran before (the user) while the client's is in `state`.
    let mut outer = s.cursors.take(&client).unwrap_or_else(|| Cursor::of(&s.state));
    outer.swap(&mut s.state);
    let prev_client = s.client.replace(client);
    let (before, epoch, events) = (s.project.clone(), s.epoch, s.events.len());
    // Others' claims: keep what's needed to take back a change made outside `Session::edit`.
    let guard = s.claims.foreign(s.client.as_deref(), &s.project).then(|| s.history.clone());
    s.sanitize_state();
    s.cursors.suspended.push(outer);
    let mut r = f(s);
    // (Gone when the command opened another project: whoever ran before starts from its state.)
    let mut outer = s.cursors.suspended.pop().unwrap_or_else(|| Cursor::of(&s.state));
    if let Some(history) = guard
        && r.is_ok()
        && let Some(e) = s.claim_conflict(&before, &s.project.clone())
    {
        s.project = before.clone();
        s.history = history;
        s.sanitize_state();
        s.bump();
        r = Err(e);
    }
    let client = std::mem::replace(&mut s.client, prev_client).unwrap_or_default();
    if s.epoch != epoch {
        // Another project was opened: the user's old cursor points into the old one.
        outer = Cursor::of(&s.state);
    }
    outer.swap(&mut s.state);
    s.cursors.put(client, outer);
    // The client opened a comp in its own cursor: the user's viewer stays where it is (the comp
    // still gets a tab).
    if events < s.events.len() {
        let tail = s.events.split_off(events);
        s.events.extend(tail.into_iter().filter(|e| !matches!(e, crate::Event::OpenComp(_))));
    }
    if !std::sync::Arc::ptr_eq(&before, &s.project) {
        s.sanitize_state();
    }
    r
}

// ---------------------------------------------------------------- claims

/// One claim: who holds the comp and until when.
#[derive(Clone, Debug)]
pub struct Claim {
    pub owner: String,
    pub expires: web_time::Instant,
    /// The lifetime it was (last) claimed for, seconds.
    pub ttl: f64,
}

/// Composition claims by clients.
#[derive(Clone, Debug, Default)]
pub struct Claims {
    pub map: BTreeMap<ItemId, Claim>,
    /// Added to the clock: tests (and hosts that want to) move time forward without waiting.
    pub clock_offset: Duration,
}

impl Claims {
    pub fn now(&self) -> web_time::Instant {
        let n = web_time::Instant::now();
        n.checked_add(self.clock_offset).unwrap_or(n)
    }

    /// Seconds left on a claim (0 once expired).
    pub fn remaining(&self, c: &Claim) -> f64 {
        c.expires.saturating_duration_since(self.now()).as_secs_f64()
    }

    /// The live claim on a comp.
    pub fn get(&self, comp: ItemId) -> Option<&Claim> {
        self.map.get(&comp).filter(|c| self.remaining(c) > 0.0)
    }

    /// Drop expired claims and claims of comps that no longer exist.
    pub fn prune(&mut self, p: &Project) {
        let now = self.now();
        self.map.retain(|id, c| c.expires > now && p.comp(*id).is_some());
    }

    /// Whether a live claim of another client than `client` exists (the user, `None`, is never
    /// blocked).
    pub fn foreign(&self, client: Option<&str>, p: &Project) -> bool {
        let Some(client) = client else { return false };
        self.map.iter().any(|(id, c)| c.owner != client && self.remaining(c) > 0.0 && p.item(*id).is_some())
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Release `client`'s claims (all, or the one on `comp`); returns the comps released.
    pub fn release(&mut self, client: &str, comp: Option<ItemId>) -> Vec<ItemId> {
        let ids: Vec<ItemId> = self.map.iter().filter(|(id, c)| c.owner == client && comp.is_none_or(|x| x == **id)).map(|(id, _)| *id).collect();
        for id in &ids {
            self.map.remove(id);
        }
        ids
    }
}

/// "4m 12s", "1h 5m", "37s".
pub fn fmt_secs(secs: f64) -> String {
    let s = if secs.is_finite() { secs.max(0.0).ceil() as u64 } else { 0 };
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

fn comp_name(p: &Project, id: ItemId) -> String {
    p.item(id).map(|i| i.name.clone()).unwrap_or_default()
}

/// Whether comp `id` differs between two project states (content, name, or deleted).
fn comp_changed(before: &Project, after: &Project, id: ItemId) -> bool {
    if !before.freeze.sections.iter().filter(|s| s.target.comp() == id).eq(after.freeze.sections.iter().filter(|s| s.target.comp() == id)) {
        return true;
    }
    match (before.comp_arc(id), after.comp_arc(id)) {
        (Some(a), Some(b)) => !(std::sync::Arc::ptr_eq(&a, &b) || a == b) || before.item(id).map(|i| &i.name) != after.item(id).map(|i| &i.name),
        (None, None) => false,
        _ => true,
    }
}

impl Session {
    /// The error for a change from `before` to `after` by the current client that touches a
    /// composition another client claimed (`None`: allowed; the user is never refused).
    pub fn claim_conflict(&self, before: &Project, after: &Project) -> Option<EngineError> {
        let client = self.client.as_deref()?;
        let (id, c) = self.claims.map.iter().find(|(id, c)| c.owner != client && self.claims.remaining(c) > 0.0 && comp_changed(before, after, **id))?;
        Some(EngineError::Other(format!(
            "composition \"{}\" is claimed by {} (expires in {}); claim it after they release it, or work on another comp",
            comp_name(before, *id),
            c.owner,
            fmt_secs(self.claims.remaining(c))
        )))
    }

    fn claims_json(&mut self) -> Vec<Value> {
        self.claims.prune(&self.project);
        let p = self.project.clone();
        self.claims
            .map
            .iter()
            .map(|(id, c)| json!({"comp": id.0, "name": comp_name(&p, *id), "client": c.owner, "expiresIn": (self.claims.remaining(c) * 10.0).round() / 10.0, "ttl": c.ttl}))
            .collect()
    }
}

/// `editor.state` additions: who is asking (`client`), the clients' cursors and the claims.
pub(crate) fn state_extras(s: &mut Session, o: &mut serde_json::Map<String, Value>) {
    if let Some(c) = &s.client {
        o.insert("client".into(), json!(c));
    }
    let clients: Vec<Value> = s
        .cursors
        .clients()
        .into_iter()
        .map(|(name, c)| {
            let time = c.active_comp.and_then(|a| c.times.get(&a)).map_or(0.0, |t| t.seconds());
            json!({"client": name, "active_comp": c.active_comp.map(|a| a.0), "time": time, "selected_layers": c.selected_layers.iter().map(|l| l.0).collect::<Vec<_>>()})
        })
        .collect();
    o.insert("clients".into(), json!(clients));
    o.insert("claims".into(), json!(s.claims_json()));
}

// ---------------------------------------------------------------- commands

fn need_client(s: &Session, cmd: &str) -> Result<String> {
    s.client.clone().ok_or_else(|| bad(cmd, "claims need a `client` (who holds the claim): add \"client\": \"<your name>\" to the params"))
}

fn claim(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "comp.claim";
    let client = need_client(s, C)?;
    let cid = comp_id(s, p)?;
    let ttl = match p.get("ttl").filter(|v| !v.is_null()) {
        None => DEFAULT_TTL,
        Some(_) => f_p(p, "ttl").filter(|t| t.is_finite() && *t > 0.0).ok_or_else(|| bad(C, "`ttl` must be a positive number of seconds"))?,
    };
    let mut warnings = vec![];
    if ttl > MAX_TTL {
        warnings.push(format!("ttl: {ttl} s is longer than the longest claim; claimed for {MAX_TTL} s (24 h)"));
    }
    let ttl = ttl.min(MAX_TTL);
    s.claims.prune(&s.project);
    if let Some(c) = s.claims.get(cid)
        && c.owner != client
    {
        return Err(EngineError::Other(format!(
            "composition \"{}\" is already claimed by {} (expires in {}); wait for them to release it, or work on another comp",
            comp_name(&s.project, cid),
            c.owner,
            fmt_secs(s.claims.remaining(c))
        )));
    }
    let now = s.claims.now();
    let expires = Duration::try_from_secs_f64(ttl).ok().and_then(|d| now.checked_add(d)).unwrap_or(now);
    let renewed = s.claims.map.contains_key(&cid);
    s.claims.map.insert(cid, Claim { owner: client.clone(), expires, ttl });
    let mut out = json!({"comp": cid.0, "name": comp_name(&s.project, cid), "client": client, "ttl": ttl, "renewed": renewed});
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

fn release(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "comp.release";
    s.claims.prune(&s.project);
    let force = b_p(p, "force").unwrap_or(false);
    let comp = match p.get("comp").filter(|v| !v.is_null()) {
        Some(_) => Some(comp_id(s, p)?),
        None => None,
    };
    let names = |s: &Session, ids: Vec<(ItemId, String)>| -> Value {
        json!({"released": ids.into_iter().map(|(id, owner)| json!({"comp": id.0, "name": comp_name(&s.project, id), "client": owner})).collect::<Vec<_>>()})
    };
    match s.client.clone() {
        Some(client) => {
            if let Some(cid) = comp
                && let Some(c) = s.claims.get(cid)
                && c.owner != client
            {
                return Err(bad(
                    C,
                    format!(
                        "composition \"{}\" is claimed by {}, not by {client}; only its owner (or the user, with `force`) can release it",
                        comp_name(&s.project, cid),
                        c.owner
                    ),
                ));
            }
            let ids = s.claims.release(&client, comp);
            Ok(names(s, ids.into_iter().map(|id| (id, client.clone())).collect()))
        }
        None if !force => Err(bad(C, "without a `client` this releases other clients' claims: pass `force: true` to do that (agents pass their `client`)")),
        None => {
            let ids: Vec<(ItemId, String)> =
                s.claims.map.iter().filter(|(id, _)| comp.is_none_or(|c| c == **id)).map(|(id, c)| (*id, c.owner.clone())).collect();
            for (id, _) in &ids {
                s.claims.map.remove(id);
            }
            Ok(names(s, ids))
        }
    }
}

fn claims(s: &mut Session, _: &Value) -> Result<Value> {
    // TEAM1: working shadows keep their sources claimed by their agents; the badges list them.
    crate::commands::shadow::refresh(s);
    let shadows = crate::commands::shadow::badges(s);
    Ok(json!({"claims": s.claims_json(), "shadows": shadows}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "comp.claim",
            label: "Claim Composition",
            menu: &[],
            shortcut: None,
            alt_shortcuts: &[],
            params: "{comp? (default: your active comp), ttl? (seconds, default 300, at most 86400)} with your `client` → {comp, name, client, ttl, renewed}: other clients can't edit the comp until you release it or the claim expires; claim again to renew",
            enabled: crate::commands::always,
            run: claim,
            journal: false,
        },
        CommandSpec {
            id: "comp.release",
            label: "Release Composition",
            menu: &[],
            shortcut: None,
            alt_shortcuts: &[],
            params: "{comp? (omitted: all your claims), force?: bool (the user, without `client`: release anyone's)} → {released: [{comp, name, client}]}",
            enabled: crate::commands::always,
            run: release,
            journal: false,
        },
        query!(
            "comp.claims",
            "Composition Claims",
            "{} → {claims: [{comp, name, client, expiresIn (s), ttl}], shadows: [{shadow, name, comp, compName, agent, status, stale, …}] (the editing team's shadows, for the comps' badges)}",
            claims
        ),
    ]
}
