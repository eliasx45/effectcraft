//! Who did what (editing harness step 2): every change of the document, tagged with who made it
//! (the user, or the agent / script `client` that ran the command), described in plain words on
//! request (`Layer › Position: key @ 6:20 [960, 540] → [900, 540]`). Agents read it with
//! `history.feed`; `agent.notices` tells an agent once what others changed since its last call
//! and when another project was opened meanwhile.
//!
//! Entries keep the project before and after (shared snapshots, as the undo history keeps them)
//! and are described only when read: recording costs one pointer comparison per change.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use effectcraft_project::{Comp, Layer, Project, Property, Uid};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use crate::history::History;
use crate::readout::{short_timecode, value_text};

/// Changes kept (older ones are dropped).
const MAX_ENTRIES: usize = 200;

/// What the data calls a change the user made (no `client`).
pub const USER: &str = "user";

/// One recorded change: the revision it led to, who made it, its undo label, the states around it.
#[derive(Clone)]
pub struct Entry {
    pub revision: u64,
    pub client: Option<String>,
    pub label: String,
    before: Arc<Project>,
    after: Arc<Project>,
}

impl Entry {
    /// The project before and after this change.
    pub(crate) fn projects(&self) -> (&Arc<Project>, &Arc<Project>) {
        (&self.before, &self.after)
    }
}

/// What a client was last told: the revision it has seen others' changes up to, and the project.
#[derive(Clone, Default)]
struct Seen {
    revision: u64,
    epoch: u64,
    path: Option<String>,
}

#[derive(Default)]
pub struct Feed {
    entries: VecDeque<Entry>,
    /// The project at the last recorded revision.
    last: Option<Arc<Project>>,
    epoch: u64,
    /// Label and merge flag of the change being made, set by [`crate::Session::edit`], undo, redo.
    pub(crate) label: Option<String>,
    pub(crate) merge: bool,
    seen: HashMap<String, Seen>,
}

impl Feed {
    /// Note the state after a revision bump. Another project (new `epoch`) starts a new feed.
    pub(crate) fn record(&mut self, revision: u64, client: Option<String>, project: &Arc<Project>, epoch: u64, history: &History) {
        let label = self.label.take();
        let merge = std::mem::take(&mut self.merge);
        if epoch != self.epoch {
            self.entries.clear();
            self.epoch = epoch;
            self.last = Some(project.clone());
            return;
        }
        // The first change of a session: the state before it is the undo step it made.
        let last = self.last.clone().or_else(|| history.undo.last().map(|(_, p)| p.clone()));
        let Some(before) = last.filter(|b| !Arc::ptr_eq(b, project)) else {
            self.last = Some(project.clone());
            return;
        };
        self.last = Some(project.clone());
        // A continuous gesture (one undo step) stays one entry.
        if merge && let Some(e) = self.entries.back_mut().filter(|e| Arc::ptr_eq(&e.after, &before) && e.client == client) {
            e.after = project.clone();
            e.revision = revision;
            return;
        }
        let label = label.or_else(|| history.undo.last().filter(|(_, p)| Arc::ptr_eq(p, &before)).map(|(l, _)| l.clone())).unwrap_or_else(|| "Change".into());
        self.entries.push_back(Entry { revision, client, label, before, after: project.clone() });
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
    }

    /// One undo group (`app.beginUndoGroup` … `endUndoGroup`) is one step: the entries `client`
    /// made since `snap` (the project when the group began) fold into one, named `label`.
    ///
    /// CDXC:Editing 2026-10-29 WHY: each command in a group bumped the revision and left its own
    /// entry, so a turn of 4 undo steps showed as "22 changes" in Ghostex and its Undo turn undid
    /// 22 times against 4 undo steps.
    pub fn fold(&mut self, snap: &Arc<Project>, label: &str, client: Option<&str>) {
        let Some(start) = self.entries.iter().rposition(|e| Arc::ptr_eq(&e.before, snap)) else { return };
        if self.entries.iter().skip(start).any(|e| e.client.as_deref() != client) {
            return;
        }
        let last = self.entries.pop_back().expect("an entry from start on");
        self.entries.truncate(start);
        self.entries.push_back(Entry { revision: last.revision, client: last.client, label: label.to_string(), before: snap.clone(), after: last.after });
    }

    /// Entries after `since` (oldest first).
    pub fn since(&self, since: u64) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.revision > since)
    }

    /// The oldest revision the feed still describes changes after.
    pub fn oldest(&self) -> Option<u64> {
        self.entries.front().map(|e| e.revision.saturating_sub(1))
    }

    /// What `client` was last told, `None` before its first `agent.notices` / `history.feed`.
    pub(crate) fn seen_revision(&self, client: &str) -> Option<u64> {
        self.seen.get(client).map(|s| s.revision)
    }

    /// Remember that `client` has seen the document up to `revision`, in project (`epoch`, `path`);
    /// returns what it had seen before.
    pub(crate) fn mark_seen(&mut self, client: &str, revision: u64, epoch: u64, path: Option<String>) -> Option<(u64, u64, Option<String>)> {
        self.seen.insert(client.to_string(), Seen { revision, epoch, path }).map(|s| (s.revision, s.epoch, s.path))
    }
}

/// Who made an entry, for text: `user`, `you` (the reading client) or the client's name.
pub fn who(e: &Entry, me: Option<&str>) -> String {
    match (&e.client, me) {
        (Some(c), Some(m)) if c == m => "you".into(),
        (Some(c), _) => c.clone(),
        (None, _) => USER.into(),
    }
}

fn comp_name(p: &Project, c: &Comp, id: effectcraft_project::ItemId) -> String {
    let _ = c;
    p.item(id).map(|i| i.name.clone()).unwrap_or_else(|| format!("comp {}", id.0))
}

/// One property's change in words (`None` when it's the same).
fn prop_change(c: &Comp, l: &Layer, b: &Property, a: &Property) -> Vec<String> {
    if a == b {
        return vec![];
    }
    let at = |t| short_timecode(c, l.comp_time(t));
    let mut out = Vec::new();
    if a.expr != b.expr {
        out.push(match (&b.expr, &a.expr) {
            (None, Some(_)) => "expression added".into(),
            (Some(_), None) => "expression removed".into(),
            _ => "expression changed".into(),
        });
    }
    if a.keys.is_empty() && b.keys.is_empty() {
        if a.value != b.value {
            out.push(format!("{} → {}", value_text(&b.value.to_json()), value_text(&a.value.to_json())));
        }
        return out;
    }
    if b.keys.is_empty() {
        out.push(format!("animated: {} keys", a.keys.len()));
        return out;
    }
    if a.keys.is_empty() {
        out.push(format!("keys removed, value {}", value_text(&a.value.to_json())));
        return out;
    }
    let times_a: Vec<_> = a.keys.iter().map(|k| k.time).collect();
    let times_b: Vec<_> = b.keys.iter().map(|k| k.time).collect();
    let ease = |keys: &[effectcraft_keyframe::Keyframe], i: usize| crate::readout::ease_fields(keys, i, a.spatial)["ease"].as_str().unwrap_or("").to_string();
    // Pairs (before index, after index) of the same key: by position when the keys only moved in
    // time, else by time.
    let mut pairs = Vec::new();
    if times_a.len() == times_b.len() && times_a != times_b {
        let moved: Vec<String> = times_b.iter().zip(&times_a).filter(|(x, y)| x != y).map(|(x, y)| format!("{} → {}", at(*x), at(*y))).collect();
        out.push(format!("key{} moved {}", if moved.len() == 1 { "" } else { "s" }, moved.join(", ")));
        pairs.extend((0..times_a.len()).map(|i| (i, i)));
    } else {
        for (i, k) in a.keys.iter().enumerate() {
            match times_b.iter().position(|t| *t == k.time) {
                Some(j) => pairs.push((j, i)),
                None => out.push(format!("+key @ {} = {}", at(k.time), value_text(&k.value.to_json()))),
            }
        }
        for k in b.keys.iter().filter(|k| !times_a.contains(&k.time)) {
            out.push(format!("−key @ {}", at(k.time)));
        }
    }
    for (j, i) in pairs {
        let (Some(o), Some(k)) = (b.keys.get(j), a.keys.get(i)) else { continue };
        if o.value != k.value {
            out.push(format!("key @ {} {} → {}", at(k.time), value_text(&o.value.to_json()), value_text(&k.value.to_json())));
        }
        let (eb, ea) = (ease(&b.keys, j), ease(&a.keys, i));
        if eb != ea {
            out.push(format!("ease @ {}: {eb} → {ea}", at(k.time)));
        }
    }
    if out.is_empty() {
        out.push("keyframes changed".into());
    }
    out
}

fn props_of(l: &Layer) -> HashMap<Uid, (&Property, String)> {
    let mut m = HashMap::new();
    l.props.walk("", &mut |path, p| {
        m.insert(p.uid, (p, path.to_string()));
    });
    m
}

/// A changed layer in words: `Name › What: before → after`.
fn layer_changes(c: &Comp, b: &Layer, a: &Layer, out: &mut Vec<String>) {
    let n = &a.name;
    let tc = |t| short_timecode(c, t);
    if a.name != b.name {
        out.push(format!("{} › renamed to \"{n}\"", b.name));
    }
    if a.in_point != b.in_point || a.out_point != b.out_point || a.start_time != b.start_time {
        out.push(format!("{n} › time {}–{} → {}–{}", tc(b.in_point), tc(b.out_point), tc(a.in_point), tc(a.out_point)));
    }
    if a.parent != b.parent {
        let name = |id: Option<effectcraft_project::LayerId>| id.and_then(|i| c.layer(i)).map(|l| l.name.clone()).unwrap_or_else(|| "none".into());
        out.push(format!("{n} › parent {} → {}", name(b.parent), name(a.parent)));
    }
    if a.track_matte != b.track_matte {
        out.push(format!("{n} › track matte changed"));
    }
    if a.switches != b.switches || a.blend_mode != b.blend_mode {
        out.push(format!("{n} › switches / mode changed"));
    }
    if a.markers != b.markers {
        out.push(format!("{n} › markers {} → {}", b.markers.len(), a.markers.len()));
    }
    if a.props == b.props {
        return;
    }
    let (pa, pb) = (props_of(a), props_of(b));
    let mut changed: Vec<(String, String)> = Vec::new();
    for (uid, (p, path)) in &pa {
        match pb.get(uid) {
            Some((o, _)) => {
                for what in prop_change(c, a, o, p) {
                    changed.push((path.clone(), format!("{n} › {}: {what}", p.name)));
                }
            }
            None => changed.push((path.clone(), String::new())),
        }
    }
    let added = changed.iter().filter(|(_, t)| t.is_empty()).count();
    let removed = pb.keys().filter(|u| !pa.contains_key(u)).count();
    changed.retain(|(_, t)| !t.is_empty());
    changed.sort();
    out.extend(changed.into_iter().map(|(_, t)| t));
    if added > 0 || removed > 0 {
        out.push(format!(
            "{n} › properties {}",
            [(added, "added"), (removed, "removed")].iter().filter(|(k, _)| *k > 0).map(|(k, w)| format!("{k} {w}")).collect::<Vec<_>>().join(", ")
        ));
    }
}

/// The ids of the comps that differ from `b` to `a` (new, changed or deleted), in id order.
pub fn changed_comps(b: &Project, a: &Project) -> Vec<u64> {
    let mut ids: Vec<u64> = a.comps().filter(|(id, c)| b.comp(**id) != Some(*c)).map(|(id, _)| id.0).collect();
    ids.extend(b.comps().filter(|(id, _)| a.comp(**id).is_none()).map(|(id, _)| id.0));
    ids.sort_unstable();
    ids
}

/// What changed from `b` to `a`, one line per change.
pub fn describe(b: &Project, a: &Project) -> Vec<String> {
    let mut out = Vec::new();
    for (id, c) in a.comps() {
        let name = comp_name(a, c, *id);
        let Some(bc) = b.comp(*id) else {
            out.push(format!("new comp \"{name}\""));
            continue;
        };
        if bc == c {
            continue;
        }
        let start = out.len();
        for la in c.layers.arcs() {
            match bc.layers.arcs().iter().find(|x| x.id == la.id) {
                None => out.push(format!("{name} › new layer \"{}\"", la.name)),
                Some(lb) if Arc::ptr_eq(lb, la) || lb == la => {}
                Some(lb) => layer_changes(c, lb, la, &mut out),
            }
        }
        for lb in bc.layers.iter().filter(|lb| c.layer(lb.id).is_none()) {
            out.push(format!("{name} › deleted layer \"{}\"", lb.name));
        }
        let order = |c: &Comp| c.layers.iter().map(|l| l.id).collect::<Vec<_>>();
        if out.len() == start && order(bc) != order(c) {
            out.push(format!("{name} › layer order changed"));
        }
        if out.len() == start {
            out.push(format!("{name} › comp settings changed"));
        }
    }
    for (id, _) in b.comps().filter(|(id, _)| a.comp(**id).is_none()) {
        out.push(format!("deleted comp \"{}\"", b.item(*id).map(|i| i.name.clone()).unwrap_or_default()));
    }
    if out.is_empty() {
        out.push("project items changed".into());
    }
    out
}

/// Most places one entry lists.
const MAX_PLACES: usize = 60;

/// Where a change shows, in its comp's time: the deleted, new or changed layer and the span the
/// change touches (moved in/out edges, changed keys with the keys either side, else the layer's
/// whole time), or the whole comp when its settings changed (`layer` null). Deleted layers come
/// from the project before, so a removal still has a place.
///
/// CDXC:Editing 2026-10-29 WHY: Ghostex's turn clips found a change by its layer's name and took
/// the whole layer's time; a name shared with the hook's video put a 3:06 removal's clip at 0:00
/// (Elias: "it's showing me the render of something unrelated"). The engine holds both states, so
/// it says exactly where.
pub fn places(b: &Project, a: &Project) -> Vec<Value> {
    let mut out = Vec::new();
    let place = |comp: u64, layer: Option<u64>, name: &str, start: Tick, end: Tick, deleted: bool| {
        let (start, end) = if end < start { (end, start) } else { (start, end) };
        let mut o = json!({"comp": comp, "layer": layer, "name": name, "start": start.seconds(), "end": end.seconds()});
        if deleted {
            o["deleted"] = json!(true);
        }
        o
    };
    for (id, c) in a.comps() {
        let Some(bc) = b.comp(*id) else { continue };
        if bc == c {
            continue;
        }
        if bc.width != c.width
            || bc.height != c.height
            || bc.duration != c.duration
            || bc.frame_rate != c.frame_rate
            || bc.background != c.background
            || bc.pixel_aspect != c.pixel_aspect
        {
            out.push(place(id.0, None, &comp_name(a, c, *id), Tick::ZERO, c.duration.max(bc.duration), false));
        }
        for la in c.layers.arcs() {
            match bc.layers.arcs().iter().find(|x| x.id == la.id) {
                None => out.push(place(id.0, Some(la.id.0), &la.name, la.in_point, la.out_point, false)),
                Some(lb) if Arc::ptr_eq(lb, la) || lb == la => {}
                Some(lb) => {
                    if let Some((start, end)) = layer_span(lb, la) {
                        out.push(place(id.0, Some(la.id.0), &la.name, start, end, false));
                    }
                }
            }
        }
        for lb in bc.layers.iter().filter(|lb| c.layer(lb.id).is_none()) {
            out.push(place(id.0, Some(lb.id.0), &lb.name, lb.in_point, lb.out_point, true));
        }
        if out.len() >= MAX_PLACES {
            break;
        }
    }
    out.truncate(MAX_PLACES);
    out
}

/// The comp-time span a layer's change touches (see [`places`]).
fn layer_span(b: &Layer, a: &Layer) -> Option<(Tick, Tick)> {
    let whole = (b.in_point.min(a.in_point), b.out_point.max(a.out_point));
    // Slid in time, re-parented, switched, renamed, re-matted, markers: everywhere it shows.
    if a.start_time != b.start_time
        || a.stretch != b.stretch
        || a.parent != b.parent
        || a.track_matte != b.track_matte
        || a.switches != b.switches
        || a.blend_mode != b.blend_mode
        || a.markers != b.markers
    {
        return Some(whole);
    }
    let mut span: Option<(Tick, Tick)> = None;
    let mut add = |t0: Tick, t1: Tick| {
        let (t0, t1) = (t0.min(t1), t0.max(t1));
        span = Some(span.map_or((t0, t1), |(s, e)| (s.min(t0), e.max(t1))));
    };
    if a.in_point != b.in_point {
        add(b.in_point, a.in_point);
    }
    if a.out_point != b.out_point {
        add(b.out_point, a.out_point);
    }
    if a.props != b.props {
        let (pa, pb) = (props_of(a), props_of(b));
        if pa.len() != pb.len() || pb.keys().any(|u| !pa.contains_key(u)) {
            return Some(whole);
        }
        for (uid, (p, _)) in &pa {
            let Some((o, _)) = pb.get(uid) else { continue };
            if *o == *p {
                continue;
            }
            // A static value or an expression shows on every frame.
            if p.keys.is_empty() || o.keys.is_empty() || p.expr != o.expr {
                return Some(whole);
            }
            // Changed keys, with the keys either side (the segments around them move too).
            let times: Vec<Tick> = {
                let mut t: Vec<Tick> = o.keys.iter().map(|k| b.comp_time(k.time)).chain(p.keys.iter().map(|k| a.comp_time(k.time))).collect();
                t.sort_unstable();
                t.dedup();
                t
            };
            let changed: Vec<Tick> = o
                .keys
                .iter()
                .filter(|k| !p.keys.contains(k))
                .map(|k| b.comp_time(k.time))
                .chain(p.keys.iter().filter(|k| !o.keys.contains(k)).map(|k| a.comp_time(k.time)))
                .collect();
            let (Some(first), Some(last)) = (changed.iter().min().copied(), changed.iter().max().copied()) else {
                // Same keys, other property settings: everywhere.
                return Some(whole);
            };
            let before = times.iter().rev().find(|t| **t < first).copied().unwrap_or(first);
            let after = times.iter().find(|t| **t > last).copied().unwrap_or(last);
            add(before.max(whole.0), after.min(whole.1));
        }
    }
    span.or(Some(whole))
}

/// An entry as JSON: revision, who (`by`: `user` or the client), label, `changes` (at most
/// `max_lines`, `more` counts the rest).
pub fn entry_json(e: &Entry, me: Option<&str>, max_lines: usize) -> Value {
    let lines = describe(&e.before, &e.after);
    let more = lines.len().saturating_sub(max_lines);
    let mut o = json!({
        "revision": e.revision, "by": e.client.as_deref().unwrap_or(USER), "who": who(e, me), "label": e.label,
        "changes": lines.into_iter().take(max_lines).collect::<Vec<_>>(),
        "comps": changed_comps(&e.before, &e.after),
    });
    if more > 0 {
        o["more"] = json!(more);
    }
    o
}

/// Entries as text lines: `user: Undo Change Property — Shape 1 › Position: …`.
pub fn entry_text(e: &Value) -> String {
    let changes: Vec<&str> = e["changes"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let more = e["more"].as_u64().map(|n| format!(" (+{n} more)")).unwrap_or_default();
    format!("{} · {}: {}{more}", e["who"].as_str().unwrap_or(""), e["label"].as_str().unwrap_or(""), changes.join("; "))
}
