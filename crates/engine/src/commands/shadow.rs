//! TEAM1: shadow compositions for the editing team (`shadow.*`).
//!
//! Ghostex EDITING runs a lead with part agents (docs/editing-team/SPEC.md in Ghostex, decided with
//! Elias on 28 Oct 2026). A part agent never edits Elias's live composition: `shadow.create` copies
//! the scene comp and every comp nested in it into the "Shadows" folder (`S1 06 ✎A2`; footage is
//! shared), the agent works there, the lead records its checks (`shadow.setStatus`), and
//! `shadow.swapIn` puts the shadow's contents into the live comp as one undo step. The live comp
//! and its nested comps keep their ids, names and uses (MAIN and every other parent); the
//! contents are exchanged, so afterwards the shadow holds what the live comp was (Before), and
//! `shadow.revert` (or undo) swaps it back.
//!
//! Elias always wins: while a shadow is worked on, its source counts as claimed by the agent
//! (`comp.claim`: other agents can't edit it), but his own edits go through. They make the shadow
//! **stale** (its source no longer matches the fingerprint taken when it was copied); a stale
//! shadow refuses to swap in without `force`, an event is logged for the host to poll
//! (`shadow.events`), and `shadow.rebase` copies his latest version again and says what he
//! changed, so the agent can redo its work on top.
//!
//! What the project file keeps (the link, fingerprints, status, checks) is
//! [`effectcraft_project::shadow`]. Review statuses are bookkeeping, not edits: `shadow.setStatus`
//! is no undo step, and [`ShadowState`] keeps the latest status so undoing an unrelated step
//! doesn't take it back (the save writes it). Staleness is checked whenever a `shadow.*` command
//! or `comp.claims` runs (hosts poll them), against fingerprints cached per comp snapshot.
//!
//! Elias, 28 Oct 2026: "since we ll work parlalely we can afford agent reviews".

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use effectcraft_color::Label;
use effectcraft_project::shadow::{CompPrint, FOLDER_NAME, LayerPrint, ShadowCheck, ShadowLink, ShadowStatus, fnv1a, hash_json};
use effectcraft_project::{Comp, Expression, ItemId, ItemKind, Layer, LayerSource, Layers, Node, Project, PropGroup, Property};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, comp_id, str_p};
use crate::{EngineError, Result, Session, cmd, query};

/// Nesting deeper than this is refused (projects come from files).
const MAX_DEPTH: usize = 64;
/// The most compositions one shadow copies.
const MAX_COMPS: usize = 2000;
/// Longest agent name (bytes).
const MAX_AGENT_LEN: usize = 64;
/// The most checks one status records, and the longest check id / note (bytes).
const MAX_CHECKS: usize = 32;
const MAX_CHECK_ID: usize = 64;
const MAX_NOTE: usize = 4000;
/// Events kept for `shadow.events` (oldest dropped).
const MAX_EVENTS: usize = 500;
/// Dropped shadows remembered for `shadow.get`.
const MAX_DROPPED: usize = 100;
/// Changed properties named per layer in a diff.
const MAX_PROPS_NAMED: usize = 24;
/// Frames `shadow.frames` renders at most, and its default size.
const MAX_FRAMES: usize = 24;
const DEFAULT_FRAME_SIDE: u64 = 480;

// ---------------------------------------------------------------- session state

/// A review status set in this session (it wins over the project's copy, which undo may roll
/// back).
#[derive(Clone, Debug, PartialEq)]
struct Note {
    status: ShadowStatus,
    checks: Vec<ShadowCheck>,
}

/// The session side of the shadows (on [`Session::shadows`]).
#[derive(Default)]
pub struct ShadowState {
    /// The project epoch this state belongs to (another project resets it).
    epoch: Option<u64>,
    notes: BTreeMap<ItemId, Note>,
    events: VecDeque<Value>,
    next_seq: u64,
    /// Whether each shadow was stale at the last check (an event marks each change).
    stale_seen: BTreeMap<ItemId, bool>,
    /// The source comps as they were copied (or left by the swap): exact property diffs.
    bases: BTreeMap<ItemId, BTreeMap<ItemId, Arc<Comp>>>,
    /// Shadows dropped in this session (`shadow.get` answers for them).
    dropped: VecDeque<(ItemId, Value)>,
    /// Fingerprints by comp, valid while the comp is this very snapshot and keeps its name.
    cache: HashMap<ItemId, (Arc<Comp>, String, CompPrint)>,
}

impl ShadowState {
    fn push_event(&mut self, mut e: Value) -> u64 {
        self.next_seq = self.next_seq.saturating_add(1);
        e["seq"] = json!(self.next_seq);
        self.events.push_back(e);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        self.next_seq
    }
}

// ---------------------------------------------------------------- fingerprints

/// `match`, or `match#n` for the n-th child with that match id (as property paths name them).
fn child_segs(g: &PropGroup) -> Vec<String> {
    let mut seen: HashMap<&str, usize> = HashMap::new();
    g.children
        .iter()
        .map(|c| {
            let n = seen.entry(c.match_id()).or_insert(0);
            *n += 1;
            if *n == 1 { c.match_id().to_string() } else { format!("{}#{n}", c.match_id()) }
        })
        .collect()
}

fn print_layer(l: &Layer) -> LayerPrint {
    let groups = child_segs(&l.props).into_iter().zip(&l.props.children).map(|(seg, c)| (seg, hash_json(c))).collect();
    LayerPrint { id: l.id.0, name: l.name.clone(), hash: hash_json(l), groups }
}

/// A composition's fingerprint (`id`, `name`: its item).
fn print_comp(id: ItemId, name: &str, comp: &Comp) -> CompPrint {
    let settings = hash_json(&Comp { layers: Layers::new(), ..comp.clone() });
    let layers: Vec<LayerPrint> = comp.layers.iter().map(print_layer).collect();
    let mut bytes = Vec::with_capacity(8 * (layers.len() + 2));
    bytes.extend_from_slice(&settings.to_le_bytes());
    bytes.extend_from_slice(&fnv1a(name.as_bytes()).to_le_bytes());
    for l in &layers {
        bytes.extend_from_slice(&l.hash.to_le_bytes());
    }
    CompPrint { comp: id, name: name.to_string(), hash: fnv1a(&bytes), settings, layers }
}

fn prints_of(p: &Project, ids: &[ItemId]) -> Vec<CompPrint> {
    ids.iter().filter_map(|id| p.item(*id).and_then(|it| it.as_comp().map(|c| print_comp(*id, &it.name, c)))).collect()
}

/// [`print_comp`] through the session's cache.
fn print_cached(st: &mut ShadowState, p: &Project, id: ItemId) -> Option<CompPrint> {
    let it = p.item(id)?;
    let arc = p.comp_arc(id)?;
    if let Some((a, n, pr)) = st.cache.get(&id)
        && Arc::ptr_eq(a, &arc)
        && *n == it.name
    {
        return Some(pr.clone());
    }
    let pr = print_comp(id, &it.name, &arc);
    st.cache.insert(id, (arc, it.name.clone(), pr.clone()));
    Some(pr)
}

// ---------------------------------------------------------------- graphs

fn comp_children(p: &Project, id: ItemId) -> Vec<ItemId> {
    let Some(c) = p.comp(id) else { return vec![] };
    let mut out = vec![];
    for l in c.layers.iter() {
        if let LayerSource::Comp { item } = &l.source
            && !out.contains(item)
        {
            out.push(*item);
        }
    }
    out
}

/// `root` and every comp nested in it (root first, depth first), not entering `stop`. Errors when
/// the nesting is deeper than [`MAX_DEPTH`] or holds more than [`MAX_COMPS`] comps.
fn graph_stopping(p: &Project, root: ItemId, stop: &BTreeSet<ItemId>) -> std::result::Result<Vec<ItemId>, String> {
    let mut out: Vec<ItemId> = vec![];
    let mut seen: BTreeSet<ItemId> = BTreeSet::new();
    let mut todo = vec![(root, 0usize)];
    while let Some((id, depth)) = todo.pop() {
        if p.comp(id).is_none() || (id != root && stop.contains(&id)) || !seen.insert(id) {
            continue;
        }
        if depth > MAX_DEPTH {
            return Err(format!("the precomps are nested more than {MAX_DEPTH} deep"));
        }
        if out.len() >= MAX_COMPS {
            return Err(format!("more than {MAX_COMPS} nested compositions"));
        }
        out.push(id);
        for c in comp_children(p, id).into_iter().rev() {
            todo.push((c, depth + 1));
        }
    }
    Ok(out)
}

fn graph(p: &Project, root: ItemId) -> std::result::Result<Vec<ItemId>, String> {
    graph_stopping(p, root, &BTreeSet::new())
}

/// Whether a comp reachable from `root` nests itself (depth-limited: too deep counts as a cycle).
fn has_cycle(p: &Project, root: ItemId) -> bool {
    // (comp, index of the next child to visit); `path` holds the comps on the current chain.
    let mut stack: Vec<(ItemId, Vec<ItemId>, usize)> = vec![(root, comp_children(p, root), 0)];
    let mut path: BTreeSet<ItemId> = BTreeSet::from([root]);
    let mut done: BTreeSet<ItemId> = BTreeSet::new();
    let mut steps = 0usize;
    while let Some((id, kids, i)) = stack.last_mut() {
        steps += 1;
        if steps > 1_000_000 {
            return true;
        }
        let Some(next) = kids.get(*i).copied() else {
            path.remove(id);
            done.insert(*id);
            stack.pop();
            continue;
        };
        *i += 1;
        if path.contains(&next) {
            return true;
        }
        if done.contains(&next) || p.comp(next).is_none() {
            continue;
        }
        if stack.len() > MAX_DEPTH {
            return true;
        }
        path.insert(next);
        let k = comp_children(p, next);
        stack.push((next, k, 0));
    }
    false
}

/// Point precomp layers and literal `comp("…")` expression references of comp `id` at other
/// comps (`items`: old → new, `names`: old → new). Only layers that use one are copied.
fn remap_comp(p: &mut Project, id: ItemId, items: &BTreeMap<ItemId, ItemId>, names: &BTreeMap<String, String>) {
    let Some(comp) = p.comp_mut(id) else { return };
    let uses = |l: &Layer| -> bool {
        if let LayerSource::Comp { item } = &l.source
            && items.contains_key(item)
        {
            return true;
        }
        let mut found = false;
        l.props.walk("", &mut |_, pr| {
            if let Some(e) = &pr.expr
                && e.text.contains("comp(")
                && names.keys().any(|n| e.text.contains(n.as_str()))
            {
                found = true;
            }
        });
        found
    };
    for l in comp.layers.iter_mut_where(uses) {
        if let LayerSource::Comp { item } = &mut l.source
            && let Some(n) = items.get(item)
        {
            *item = *n;
        }
        l.props.walk_mut(&mut |pr| {
            if let Some(e) = &mut pr.expr {
                let renamed = rename_refs(&e.text, names);
                if renamed != e.text {
                    *e = Expression { text: renamed, enabled: e.enabled };
                }
            }
        });
    }
}

/// `comp("Name")` / `comp('Name')` renamed by `names` (old → new), as Deep Duplicate does.
fn rename_refs(text: &str, names: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();
    for (old, new) in names {
        for q in ['"', '\''] {
            let esc = |n: &str| n.replace('\\', "\\\\").replace(q, &format!("\\{q}"));
            out = out.replace(&format!("comp({q}{}{q})", esc(old)), &format!("comp({q}{}{q})", esc(new)));
        }
    }
    out
}

fn item_name(p: &Project, id: ItemId) -> String {
    p.item(id).map(|i| i.name.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------- diffs

/// Every property of a group: uid path → (name path, property).
fn leaves<'a>(g: &'a PropGroup, uid_prefix: &str, name_prefix: &str, depth: usize, out: &mut BTreeMap<String, (String, &'a Property)>) {
    if depth > MAX_DEPTH {
        return;
    }
    for c in &g.children {
        let up = format!("{uid_prefix}@{}", c.uid());
        match c {
            Node::Prop(pr) => {
                out.insert(up, (format!("{name_prefix}{}", pr.name), pr));
            }
            Node::Group(sub) => leaves(sub, &format!("{up}/"), &format!("{name_prefix}{} › ", sub.name), depth + 1, out),
        }
    }
}

/// The properties that differ between two versions of one layer, by name path.
fn changed_props(a: &Layer, b: &Layer) -> (Vec<String>, usize) {
    let (mut la, mut lb) = (BTreeMap::new(), BTreeMap::new());
    leaves(&a.props, "", "", 0, &mut la);
    leaves(&b.props, "", "", 0, &mut lb);
    let mut names: Vec<String> = vec![];
    let mut n = 0usize;
    for (k, (name, pa)) in &la {
        let differs = match lb.get(k) {
            Some((_, pb)) => pa != pb,
            None => true,
        };
        if differs {
            n += 1;
            if names.len() < MAX_PROPS_NAMED {
                names.push(if lb.contains_key(k) { name.clone() } else { format!("{name} (removed)") });
            }
        }
    }
    for (k, (name, _)) in &lb {
        if !la.contains_key(k) {
            n += 1;
            if names.len() < MAX_PROPS_NAMED {
                names.push(format!("{name} (added)"));
            }
        }
    }
    (names, n)
}

/// What changed from `base` to `now` (fingerprints of the same comps, by comp id); `arcs`: the
/// base comps themselves when this session has them (property-level detail), `p`: the project as
/// it is now.
fn diff(base: &[CompPrint], now: &[CompPrint], arcs: Option<&BTreeMap<ItemId, Arc<Comp>>>, p: &Project) -> Value {
    let mut comps = vec![];
    let mut lines: Vec<String> = vec![];
    let base_ids: BTreeSet<ItemId> = base.iter().map(|c| c.comp).collect();
    let now_ids: BTreeSet<ItemId> = now.iter().map(|c| c.comp).collect();
    let added_comps: Vec<Value> = now.iter().filter(|c| !base_ids.contains(&c.comp)).map(|c| json!({"comp": c.comp.0, "name": c.name})).collect();
    let removed_comps: Vec<Value> = base.iter().filter(|c| !now_ids.contains(&c.comp)).map(|c| json!({"comp": c.comp.0, "name": c.name})).collect();
    for b in base {
        let Some(n) = now.iter().find(|c| c.comp == b.comp) else { continue };
        if n.hash == b.hash {
            continue;
        }
        let mut what = vec![];
        let settings = n.settings != b.settings;
        let renamed = n.name != b.name;
        let mut added = vec![];
        let mut removed = vec![];
        let mut changed = vec![];
        for lb in &b.layers {
            match n.layers.iter().find(|l| l.id == lb.id) {
                None => removed.push(json!({"layer": lb.id, "name": lb.name})),
                Some(ln) if ln.hash != lb.hash => {
                    let mut parts: Vec<String> = vec![];
                    for (seg, h) in &ln.groups {
                        if lb.groups.iter().find(|(s, _)| s == seg).is_none_or(|(_, hb)| hb != h) {
                            parts.push(seg.clone());
                        }
                    }
                    for (seg, _) in &lb.groups {
                        if !ln.groups.iter().any(|(s, _)| s == seg) {
                            parts.push(format!("{seg} (removed)"));
                        }
                    }
                    if parts.is_empty() || ln.name != lb.name {
                        parts.push("layer (name, timing, switches…)".into());
                    }
                    let mut entry = json!({"layer": ln.id, "name": ln.name, "parts": parts});
                    let la = arcs.and_then(|m| m.get(&b.comp)).and_then(|c| c.layers.iter().find(|l| l.id.0 == lb.id).cloned());
                    let lnow = p.comp(b.comp).and_then(|c| c.layers.iter().find(|l| l.id.0 == ln.id).cloned());
                    if let (Some(la), Some(lnow)) = (la, lnow) {
                        let (props, count) = changed_props(&la, &lnow);
                        entry["props"] = json!(props);
                        entry["propCount"] = json!(count);
                    }
                    changed.push(entry);
                }
                Some(_) => {}
            }
        }
        for ln in &n.layers {
            if !b.layers.iter().any(|l| l.id == ln.id) {
                added.push(json!({"layer": ln.id, "name": ln.name}));
            }
        }
        let order_only = added.is_empty() && removed.is_empty() && changed.is_empty() && !settings && !renamed;
        if renamed {
            what.push(format!("renamed from \"{}\"", b.name));
        }
        if settings {
            what.push("composition settings or markers".to_string());
        }
        if !changed.is_empty() {
            let names: Vec<String> = changed
                .iter()
                .take(6)
                .map(|c| {
                    let parts: Vec<&str> = c["parts"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    format!("{} ({})", c["name"].as_str().unwrap_or(""), parts.join(", "))
                })
                .collect();
            let more = if changed.len() > 6 { format!(" and {} more", changed.len() - 6) } else { String::new() };
            what.push(format!("{} changed: {}{more}", plural(changed.len(), "layer"), names.join("; ")));
        }
        if !added.is_empty() {
            what.push(format!("{} added", plural(added.len(), "layer")));
        }
        if !removed.is_empty() {
            what.push(format!("{} removed", plural(removed.len(), "layer")));
        }
        if order_only {
            what.push("layer order".to_string());
        }
        lines.push(format!("{}: {}", n.name, what.join("; ")));
        comps.push(json!({
            "comp": b.comp.0, "name": n.name, "settings": settings, "renamed": renamed, "layerOrder": order_only,
            "added": added, "removed": removed, "changed": changed,
        }));
    }
    for c in &added_comps {
        lines.push(format!("{}: newly nested", c["name"].as_str().unwrap_or("")));
    }
    for c in &removed_comps {
        lines.push(format!("{}: no longer nested", c["name"].as_str().unwrap_or("")));
    }
    let changed = !comps.is_empty() || !added_comps.is_empty() || !removed_comps.is_empty();
    json!({
        "changed": changed,
        "summary": if changed { lines.join(" · ") } else { "no changes".to_string() },
        "comps": comps, "addedComps": added_comps, "removedComps": removed_comps,
        "exact": arcs.is_some(),
    })
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

// ---------------------------------------------------------------- refresh

/// The live comp's fingerprints now (the comps nested in it now, through the cache).
fn now_prints(st: &mut ShadowState, p: &Project, source: ItemId) -> Vec<CompPrint> {
    let ids = graph(p, source).unwrap_or_else(|_| vec![source]);
    ids.into_iter().filter_map(|id| print_cached(st, p, id)).collect()
}

fn is_stale(base: &[CompPrint], now: &[CompPrint]) -> bool {
    base.len() != now.len() || base.iter().any(|b| now.iter().find(|n| n.comp == b.comp).is_none_or(|n| n.hash != b.hash))
}

/// The review status in effect (this session's, else the project's).
fn status_of(s: &Session, l: &ShadowLink) -> (ShadowStatus, Vec<ShadowCheck>) {
    match s.shadows.notes.get(&l.shadow) {
        Some(n) => (n.status, n.checks.clone()),
        None => (l.status, l.checks.clone()),
    }
}

fn status_name(s: &Session, l: &ShadowLink) -> &'static str {
    if l.swapped { "swapped" } else { status_of(s, l).0.id() }
}

/// Whether the shadow is being worked on (its source counts as claimed by its agent).
fn active(s: &Session, l: &ShadowLink) -> bool {
    !l.swapped && matches!(status_of(s, l).0, ShadowStatus::Working | ShadowStatus::Checking)
}

/// Change the project's shadow bookkeeping without an undo step (statuses and checks): the
/// document is modified (it needs saving), the step author of the current state is kept.
fn write_links(s: &mut Session, f: impl FnOnce(&mut Vec<ShadowLink>)) {
    let mut links = s.project.shadows.clone();
    f(&mut links);
    if links == s.project.shadows {
        return;
    }
    let author = s.history.author(&s.project).map(str::to_string);
    Arc::make_mut(&mut s.project).shadows = links;
    s.history.set_author(&s.project, author);
    s.bump();
}

/// Bring the session side up to date: reset it for another project, write this session's
/// statuses back into the project if an undo rolled them back, keep the working shadows' sources
/// claimed by their agents, and log stale / fresh changes. Every `shadow.*` command and
/// `comp.claims` run it first.
pub fn refresh(s: &mut Session) {
    if s.shadows.epoch != Some(s.epoch) {
        let seq = s.shadows.next_seq;
        s.shadows = ShadowState { epoch: Some(s.epoch), next_seq: seq, ..ShadowState::default() };
    }
    // Statuses: this session's win (an undo of an earlier step may have rolled them back).
    let notes = s.shadows.notes.clone();
    write_links(s, |links| {
        for l in links.iter_mut() {
            if let Some(n) = notes.get(&l.shadow) {
                l.status = n.status;
                l.checks.clone_from(&n.checks);
            }
        }
    });
    let keep: BTreeSet<ItemId> = s.project.shadows.iter().flat_map(|l| l.copies.iter().map(|(a, _)| *a)).collect();
    s.shadows.cache.retain(|id, _| keep.contains(id));
    sync_claims(s);
    // Stale: compare each source with its fingerprint.
    let links = s.project.shadows.clone();
    let p = s.project.clone();
    for l in &links {
        if p.comp(l.source).is_none() || p.comp(l.shadow).is_none() {
            continue;
        }
        let now = now_prints(&mut s.shadows, &p, l.source);
        let stale = is_stale(&l.base, &now);
        let seen = s.shadows.stale_seen.get(&l.shadow).copied().unwrap_or(false);
        if stale != seen {
            let mut e = event_base(&p, l, if stale { "stale" } else { "fresh" });
            if stale {
                let d = diff(&l.base, &now, s.shadows.bases.get(&l.shadow), &p);
                e["summary"] = d["summary"].clone();
                e["message"] = json!(if l.swapped {
                    format!("{} was edited after {}'s shadow was swapped in: shadow.revert needs force", item_name(&p, l.source), l.agent)
                } else {
                    format!("{} was edited while {} works on its shadow: the agent should shadow.rebase", item_name(&p, l.source), l.agent)
                });
            }
            e["revision"] = json!(s.revision);
            s.shadows.push_event(e);
            s.shadows.stale_seen.insert(l.shadow, stale);
        }
    }
}

/// Working shadows' sources are claimed by their agents (renewed as they go; another client's
/// own claim is left alone); a source no working shadow needs any more is released.
fn sync_claims(s: &mut Session) {
    let links = s.project.shadows.clone();
    let now = s.claims.now();
    let mut wanted: BTreeMap<ItemId, String> = BTreeMap::new();
    for l in &links {
        if active(s, l) && s.project.comp(l.source).is_some() {
            wanted.entry(l.source).or_insert_with(|| l.agent.clone());
        }
    }
    for (comp, agent) in &wanted {
        let held = s.claims.get(*comp).map(|c| (c.owner.clone(), s.claims.remaining(c)));
        match held {
            // Another client's claim (another agent's shadow, or its own comp.claim) stays.
            Some((owner, _)) if &owner != agent => {}
            Some((_, left)) if left > crate::agents::MAX_TTL / 2.0 => {}
            _ => {
                let ttl = crate::agents::MAX_TTL;
                let expires = std::time::Duration::try_from_secs_f64(ttl).ok().and_then(|d| now.checked_add(d)).unwrap_or(now);
                s.claims.map.insert(*comp, crate::agents::Claim { owner: agent.clone(), expires, ttl });
            }
        }
    }
    // Release claims shadow agents hold on sources none of their shadows works on any more.
    let agents: BTreeSet<&str> = links.iter().map(|l| l.agent.as_str()).collect();
    let sources: BTreeSet<ItemId> = links.iter().map(|l| l.source).collect();
    let stale: Vec<ItemId> = s
        .claims
        .map
        .iter()
        .filter(|(id, c)| sources.contains(id) && agents.contains(c.owner.as_str()) && wanted.get(id) != Some(&c.owner))
        .filter(|(id, c)| !links.iter().any(|l| l.source == **id && l.agent == c.owner && active(s, l)))
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        // Only claims a shadow made (24 h): a short claim the agent took itself stays.
        if s.claims.map.get(&id).is_some_and(|c| c.ttl >= crate::agents::MAX_TTL) {
            s.claims.map.remove(&id);
        }
    }
}

fn event_base(p: &Project, l: &ShadowLink, kind: &str) -> Value {
    json!({"kind": kind, "shadow": l.shadow.0, "name": item_name(p, l.shadow), "comp": l.source.0, "compName": item_name(p, l.source), "agent": l.agent})
}

fn log(s: &mut Session, l: &ShadowLink, kind: &str, extra: Value) {
    let mut e = event_base(&s.project, l, kind);
    if let (Some(o), Some(x)) = (e.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            o.insert(k.clone(), v.clone());
        }
    }
    e["revision"] = json!(s.revision);
    s.shadows.push_event(e);
}

// ---------------------------------------------------------------- JSON

fn link_json(s: &mut Session, l: &ShadowLink, detail: bool) -> Value {
    let p = s.project.clone();
    let (status, checks) = status_of(s, l);
    let now = now_prints(&mut s.shadows, &p, l.source);
    let stale = p.comp(l.source).is_some() && is_stale(&l.base, &now);
    let broken = p.comp(l.source).is_none() || p.comp(l.shadow).is_none();
    let claim = s.claims.get(l.source).map(|c| c.owner.clone());
    let mut o = json!({
        "shadow": l.shadow.0,
        "name": item_name(&p, l.shadow),
        "comp": l.source.0,
        "compName": item_name(&p, l.source),
        "agent": l.agent,
        "status": status_name(s, l),
        "reviewStatus": status.id(),
        "checks": checks.iter().map(|c| json!({"id": c.id, "ok": c.ok, "note": c.note})).collect::<Vec<_>>(),
        "checksPassed": !checks.is_empty() && checks.iter().all(|c| c.ok),
        "swapped": l.swapped,
        "swaps": l.swaps,
        "stale": stale,
        "claimedBy": claim,
        "folder": l.folder.map(|f| f.0),
        "baseRevision": l.base_revision,
    });
    if broken {
        o["broken"] = json!(true);
        o["message"] = json!("the live comp or the shadow comp is gone (deleted, or a step was undone): shadow.drop it");
    }
    // Before / After: which comp holds which right now.
    let (before, after) = if l.swapped { (l.shadow, l.source) } else { (l.source, l.shadow) };
    o["before"] = json!(before.0);
    o["after"] = json!(after.0);
    if stale || detail {
        let d = diff(&l.base, &now, s.shadows.bases.get(&l.shadow), &p);
        if detail {
            o["changes"] = d;
        } else {
            o["changes"] = json!({"summary": d["summary"], "comps": d["comps"]});
        }
    }
    if detail {
        o["copies"] = json!(
            l.copies.iter().map(|(a, b)| json!({"live": a.0, "liveName": item_name(&p, *a), "copy": b.0, "copyName": item_name(&p, *b)})).collect::<Vec<_>>()
        );
    }
    o
}

/// The project to save: this session's review statuses written into its shadow links.
pub fn for_save(s: &Session) -> Arc<Project> {
    let differs = s.project.shadows.iter().any(|l| s.shadows.notes.get(&l.shadow).is_some_and(|n| n.status != l.status || n.checks != l.checks));
    if !differs || s.shadows.epoch != Some(s.epoch) {
        return s.project.clone();
    }
    let mut p = (*s.project).clone();
    for l in p.shadows.iter_mut() {
        if let Some(n) = s.shadows.notes.get(&l.shadow) {
            l.status = n.status;
            l.checks.clone_from(&n.checks);
        }
    }
    Arc::new(p)
}

/// The shadows a `comp.claims` reply lists for the UI's badges.
pub fn badges(s: &mut Session) -> Vec<Value> {
    let links: Vec<ShadowLink> = s.project.shadows.iter().filter(|l| s.project.comp(l.source).is_some()).cloned().collect();
    links.iter().map(|l| link_json(s, l, false)).collect()
}

// ---------------------------------------------------------------- params

/// `shadow`: id (number or digits) or the shadow comp's name → index in `project.shadows`.
fn shadow_index(s: &Session, p: &Value, cmd: &str) -> Result<usize> {
    let v = p
        .get("shadow")
        .filter(|v| !v.is_null())
        .ok_or_else(|| bad(cmd, "`shadow` is required (the shadow comp's id or name, from shadow.create or shadow.list)"))?;
    let id = match v {
        Value::Number(n) => n.as_u64().map(ItemId),
        Value::String(t) => {
            let by_name: Vec<ItemId> = s.project.shadows.iter().filter(|l| s.project.item(l.shadow).is_some_and(|i| &i.name == t)).map(|l| l.shadow).collect();
            match by_name.len() {
                1 => by_name.first().copied(),
                0 => t.trim().parse::<u64>().ok().map(ItemId),
                _ => return Err(bad(cmd, format!("several shadows are named {t:?}: use the id"))),
            }
        }
        _ => None,
    }
    .ok_or_else(|| bad(cmd, format!("`shadow` must be a shadow comp id or name, got {v}")))?;
    s.project.shadows.iter().position(|l| l.shadow == id).ok_or_else(|| {
        let dropped = s.shadows.dropped.iter().any(|(d, _)| *d == id);
        if dropped { bad(cmd, format!("shadow {} was dropped", id.0)) } else { bad(cmd, format!("no shadow {} (shadow.list shows them)", id.0)) }
    })
}

fn link_at(s: &Session, i: usize, cmd: &str) -> Result<ShadowLink> {
    s.project.shadows.get(i).cloned().ok_or_else(|| bad(cmd, "the shadow vanished"))
}

fn agent_p(s: &Session, p: &Value, cmd: &str) -> Result<String> {
    let a = match p.get("agent").filter(|v| !v.is_null()) {
        Some(Value::String(a)) => a.trim().to_string(),
        Some(v) => return Err(bad(cmd, format!("`agent` must be a name, got {v}"))),
        None => s.client.clone().unwrap_or_default(),
    };
    if a.is_empty() {
        return Err(bad(cmd, "`agent` is required (who works on the shadow, e.g. \"A2\"), or run it with your `client`"));
    }
    if a.len() > MAX_AGENT_LEN {
        return Err(bad(cmd, format!("`agent` is longer than {MAX_AGENT_LEN} bytes")));
    }
    if a.chars().any(char::is_control) {
        return Err(bad(cmd, "`agent` must not hold control characters"));
    }
    Ok(a)
}

/// `checks: [{id, ok, note?}]`.
fn checks_p(v: &Value, cmd: &str) -> Result<Vec<ShadowCheck>> {
    let arr = v.as_array().ok_or_else(|| bad(cmd, "`checks` must be a list of {id, ok, note?}"))?;
    if arr.len() > MAX_CHECKS {
        return Err(bad(cmd, format!("at most {MAX_CHECKS} checks")));
    }
    let mut out: Vec<ShadowCheck> = vec![];
    for (i, c) in arr.iter().enumerate() {
        let id = c
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| bad(cmd, format!("checks[{i}]: `id` (a name such as \"lint\") is required")))?;
        if id.len() > MAX_CHECK_ID {
            return Err(bad(cmd, format!("checks[{i}]: `id` is longer than {MAX_CHECK_ID} bytes")));
        }
        let ok = c.get("ok").and_then(Value::as_bool).ok_or_else(|| bad(cmd, format!("checks[{i}]: `ok` must be true or false")))?;
        let note = match c.get("note").filter(|v| !v.is_null()) {
            None => String::new(),
            Some(Value::String(n)) => n.clone(),
            Some(v) => return Err(bad(cmd, format!("checks[{i}]: `note` must be text, got {v}"))),
        };
        if note.len() > MAX_NOTE {
            return Err(bad(cmd, format!("checks[{i}]: `note` is longer than {MAX_NOTE} bytes")));
        }
        match out.iter_mut().find(|x| x.id == id) {
            Some(x) => {
                x.ok = ok;
                x.note = note;
            }
            None => out.push(ShadowCheck { id: id.to_string(), ok, note }),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- copy

/// Copy the comps of `graph` (live → copy, reusing the copies in `reuse` that still exist),
/// named `<name> ✎<agent>`, in `folder`; the copies use each other. Returns the pairs in graph
/// order.
fn copy_graph(proj: &mut Project, graph: &[ItemId], agent: &str, folder: ItemId, reuse: &[(ItemId, ItemId)]) -> Result<Vec<(ItemId, ItemId)>> {
    let mut items: BTreeMap<ItemId, ItemId> = BTreeMap::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut pairs = vec![];
    for id in graph {
        let it = proj.item(*id).ok_or_else(|| EngineError::Other("a composition vanished while copying".into()))?.clone();
        let reused = reuse.iter().find(|(l, _)| l == id).map(|(_, c)| *c).filter(|c| proj.comp(*c).is_some());
        let cid = match reused {
            Some(c) => {
                if let Some(slot) = proj.item_mut(c) {
                    slot.kind = it.kind.clone();
                }
                c
            }
            None => {
                let name = proj.unique_item_name(&format!("{} ✎{agent}", it.name), |_| false);
                proj.add_item(&name, it.label, Some(folder), it.kind.clone())
            }
        };
        names.insert(it.name.clone(), item_name(proj, cid));
        items.insert(*id, cid);
        pairs.push((*id, cid));
    }
    super::comp_tools::relink_copies(proj, &items, &names);
    Ok(pairs)
}

/// The shadows' folder: the one the shadows already use, else a "Shadows" folder at the root
/// (made if there is none).
fn folder_for(proj: &mut Project) -> ItemId {
    if let Some(f) = proj.shadows.iter().filter_map(|l| l.folder).find(|f| proj.item(*f).is_some_and(|i| i.is_folder())) {
        return f;
    }
    if let Some(f) = proj.items.values().find(|i| i.is_folder() && i.parent.is_none() && i.name == FOLDER_NAME).map(|i| i.id) {
        return f;
    }
    proj.add_item(FOLDER_NAME, Label::None, None, ItemKind::Folder)
}

/// Delete items (and layers elsewhere that use them, and their Render Queue entries).
fn delete_items(proj: &mut Project, doomed: &BTreeSet<ItemId>) {
    for id in doomed {
        proj.items.remove(id);
    }
    let users: Vec<ItemId> =
        proj.comps().filter(|(_, c)| c.layers.iter().any(|l| l.source.item().is_some_and(|i| doomed.contains(&i)))).map(|(id, _)| *id).collect();
    for cid in users {
        if let Some(comp) = proj.comp_mut(cid) {
            let gone: Vec<effectcraft_project::LayerId> =
                comp.layers.iter().filter(|l| l.source.item().is_some_and(|i| doomed.contains(&i))).map(|l| l.id).collect();
            comp.layers.retain(|l| !gone.contains(&l.id));
            let refers = |l: &Layer| l.parent.is_some_and(|x| gone.contains(&x)) || l.track_matte.is_some_and(|m| gone.contains(&m.layer));
            for l in comp.layers.iter_mut_where(refers) {
                if l.parent.is_some_and(|x| gone.contains(&x)) {
                    l.parent = None;
                }
                if l.track_matte.is_some_and(|m| gone.contains(&m.layer)) {
                    l.track_matte = None;
                }
            }
        }
    }
    proj.render_queue.retain(|r| !doomed.contains(&r.comp));
}

// ---------------------------------------------------------------- swap

/// Exchange the contents of the live comps and their copies (the shadow at `i`): the live comps
/// keep their ids, names and uses; precomp layers and `comp("…")` references follow; comps the
/// agent made inside the shadow move with its contents (out of the Shadows folder). Refused, with
/// nothing changed, if the result would nest a comp in itself.
fn swap(proj: &mut Project, i: usize, cmd: &str) -> Result<()> {
    let link = proj.shadows.get(i).cloned().ok_or_else(|| bad(cmd, "the shadow vanished"))?;
    let pairs: Vec<(ItemId, ItemId)> = link.copies.iter().copied().filter(|(a, b)| proj.comp(*a).is_some() && proj.comp(*b).is_some()).collect();
    if !pairs.iter().any(|(a, b)| *a == link.source && *b == link.shadow) {
        return Err(bad(cmd, "the live comp or the shadow comp is gone: nothing to swap"));
    }
    let to_live: BTreeMap<ItemId, ItemId> = pairs.iter().map(|(a, b)| (*b, *a)).collect();
    let to_copy: BTreeMap<ItemId, ItemId> = pairs.iter().copied().collect();
    let names_live: BTreeMap<String, String> = pairs.iter().map(|(a, b)| (item_name(proj, *b), item_name(proj, *a))).collect();
    let names_copy: BTreeMap<String, String> = pairs.iter().map(|(a, b)| (item_name(proj, *a), item_name(proj, *b))).collect();
    let lives: BTreeSet<ItemId> = to_copy.keys().copied().collect();
    let copies: BTreeSet<ItemId> = to_live.keys().copied().collect();
    // Comps outside the pairs each side uses (made by the agent, or by Elias), before the swap.
    let extra_shadow: BTreeSet<ItemId> =
        graph_stopping(proj, link.shadow, &lives).map_err(|m| bad(cmd, m))?.into_iter().filter(|c| !copies.contains(c)).collect();
    let extra_live: BTreeSet<ItemId> =
        graph_stopping(proj, link.source, &copies).map_err(|m| bad(cmd, m))?.into_iter().filter(|c| !lives.contains(c)).collect();
    for (a, b) in &pairs {
        let (Some(ca), Some(cb)) = (proj.comp_arc(*a), proj.comp_arc(*b)) else { continue };
        if let Some(it) = proj.item_mut(*a) {
            it.kind = ItemKind::Comp(cb);
        }
        if let Some(it) = proj.item_mut(*b) {
            it.kind = ItemKind::Comp(ca);
        }
    }
    for a in &lives {
        remap_comp(proj, *a, &to_live, &names_live);
    }
    for b in &copies {
        remap_comp(proj, *b, &to_copy, &names_copy);
    }
    let source_folder = proj.item(link.source).and_then(|it| it.parent);
    // The shadow side's own comps now belong to the live comp, and the other way round.
    // (A comp also used outside its side, e.g. straight from MAIN, is left as it is.)
    let within = |proj: &Project, x: ItemId, side: &BTreeSet<ItemId>, extra: &BTreeSet<ItemId>| {
        proj.item_users(x).iter().all(|(u, _)| side.contains(u) || extra.contains(u))
    };
    for x in extra_shadow.difference(&extra_live) {
        if !within(proj, *x, &lives, &extra_shadow) {
            continue;
        }
        remap_comp(proj, *x, &to_live, &names_live);
        if link.folder.is_some()
            && let Some(it) = proj.item_mut(*x)
            && it.parent == link.folder
        {
            it.parent = source_folder;
        }
    }
    for x in extra_live.difference(&extra_shadow) {
        if !within(proj, *x, &copies, &extra_live) {
            continue;
        }
        remap_comp(proj, *x, &to_copy, &names_copy);
    }
    if has_cycle(proj, link.source) || has_cycle(proj, link.shadow) {
        return Err(bad(cmd, "the swap would nest a composition inside itself (does the shadow use its own live comp as a precomp?)"));
    }
    let base_ids = graph(proj, link.source).map_err(|m| bad(cmd, m))?;
    let base = prints_of(proj, &base_ids);
    let slot = proj.shadows.get_mut(i).ok_or_else(|| bad(cmd, "the shadow vanished"))?;
    slot.swapped = !slot.swapped;
    slot.swaps = slot.swaps.saturating_add(1);
    slot.base = base;
    Ok(())
}

/// The live comp's nested comps as they are now (for exact diffs later).
fn snapshot(p: &Project, source: ItemId) -> BTreeMap<ItemId, Arc<Comp>> {
    graph(p, source).unwrap_or_default().into_iter().filter_map(|id| p.comp_arc(id).map(|c| (id, c))).collect()
}

/// Lift the claims shadow agents hold on `comp` (a swap is the team's own edit); returns them to
/// put back if the swap fails.
fn lift_claims(s: &mut Session, comp: ItemId) -> Option<crate::agents::Claim> {
    let owner = s.claims.map.get(&comp)?.owner.clone();
    if !s.project.shadows.iter().any(|l| l.source == comp && l.agent == owner) {
        return None;
    }
    s.claims.map.remove(&comp)
}

// ---------------------------------------------------------------- commands

fn create(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.create";
    refresh(s);
    let agent = agent_p(s, p, C)?;
    let source = comp_id(s, p)?;
    if let Some(l) = s.project.shadows.iter().find(|l| l.owns(source)) {
        return Err(bad(
            C,
            format!("\"{}\" is a shadow comp ({}'s): make the shadow from its live comp {}", item_name(&s.project, source), l.agent, l.source.0),
        ));
    }
    if let Some(l) = s.project.shadows.iter().find(|l| l.source == source && l.agent == agent && !l.swapped) {
        return Err(bad(
            C,
            format!(
                "{agent} already has a shadow of \"{}\": {} (\"{}\"); keep working on it, shadow.rebase it, or shadow.drop it first",
                item_name(&s.project, source),
                l.shadow.0,
                item_name(&s.project, l.shadow)
            ),
        ));
    }
    let ids = graph(&s.project, source).map_err(|m| bad(C, m))?;
    let revision = s.revision;
    let label = format!("Create Shadow ✎{agent}");
    let link = s.edit(&label, None, |proj, _| {
        let folder = folder_for(proj);
        let pairs = copy_graph(proj, &ids, &agent, folder, &[])?;
        let shadow = pairs.first().map(|(_, c)| *c).ok_or_else(|| EngineError::Other("nothing was copied".into()))?;
        let link = ShadowLink {
            shadow,
            source,
            agent: agent.clone(),
            folder: Some(folder),
            copies: pairs,
            base: prints_of(proj, &ids),
            base_revision: revision,
            status: ShadowStatus::Working,
            checks: vec![],
            swapped: false,
            swaps: 0,
        };
        proj.shadows.push(link.clone());
        Ok(link)
    })?;
    s.shadows.bases.insert(link.shadow, snapshot(&s.project, source));
    s.shadows.notes.insert(link.shadow, Note { status: ShadowStatus::Working, checks: vec![] });
    s.shadows.stale_seen.insert(link.shadow, false);
    log(s, &link, "created", json!({}));
    sync_claims(s);
    let mut out = link_json(s, &link, true);
    out["created"] = json!(true);
    Ok(out)
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.list";
    refresh(s);
    let comp = match p.get("comp").filter(|v| !v.is_null()) {
        Some(_) => Some(comp_id(s, p)?),
        None => None,
    };
    let agent = match p.get("agent").filter(|v| !v.is_null()) {
        Some(Value::String(a)) => Some(a.trim().to_string()),
        Some(v) => return Err(bad(C, format!("`agent` must be a name, got {v}"))),
        None => None,
    };
    let links: Vec<ShadowLink> = s
        .project
        .shadows
        .iter()
        .filter(|l| comp.is_none_or(|c| l.source == c || l.owns(c)))
        .filter(|l| agent.as_deref().is_none_or(|a| l.agent == a))
        .cloned()
        .collect();
    let shadows: Vec<Value> = links.iter().map(|l| link_json(s, l, false)).collect();
    let dropped: Vec<Value> = s
        .shadows
        .dropped
        .iter()
        .filter(|(_, v)| comp.is_none_or(|c| v["comp"].as_u64() == Some(c.0)))
        .filter(|(_, v)| agent.as_deref().is_none_or(|a| v["agent"].as_str() == Some(a)))
        .map(|(_, v)| v.clone())
        .collect();
    Ok(json!({"shadows": shadows, "dropped": dropped}))
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.get";
    refresh(s);
    match shadow_index(s, p, C) {
        Ok(i) => {
            let l = link_at(s, i, C)?;
            Ok(link_json(s, &l, true))
        }
        Err(e) => {
            // A shadow dropped in this session still answers (status "dropped").
            let id = p.get("shadow").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|t| t.trim().parse().ok())));
            match id.and_then(|id| s.shadows.dropped.iter().find(|(d, _)| d.0 == id)) {
                Some((_, v)) => Ok(v.clone()),
                None => Err(e),
            }
        }
    }
}

fn set_status(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.setStatus";
    refresh(s);
    let i = shadow_index(s, p, C)?;
    let l = link_at(s, i, C)?;
    let name = str_p(p, "status").ok_or_else(|| bad(C, "`status` is required: working, checking, passed or failed"))?;
    let status = ShadowStatus::from_name(name).ok_or_else(|| {
        let hint = match name.trim() {
            "swapped" => " (shadow.swapIn swaps it in)",
            "dropped" => " (shadow.drop drops it)",
            _ => "",
        };
        bad(C, format!("`status` must be working, checking, passed or failed, not {name:?}{hint}"))
    })?;
    if l.swapped {
        return Err(bad(C, "this shadow is swapped in: shadow.revert it first, or make a new shadow"));
    }
    let (_, old_checks) = status_of(s, &l);
    let checks = match p.get("checks").filter(|v| !v.is_null()) {
        Some(v) => checks_p(v, C)?,
        None => old_checks,
    };
    let mut warnings: Vec<String> = vec![];
    if status == ShadowStatus::Passed && checks.iter().any(|c| !c.ok) {
        warnings.push("status passed with a failed check".into());
    }
    s.shadows.notes.insert(l.shadow, Note { status, checks: checks.clone() });
    let shadow = l.shadow;
    write_links(s, |links| {
        if let Some(x) = links.iter_mut().find(|x| x.shadow == shadow) {
            x.status = status;
            x.checks.clone_from(&checks);
        }
    });
    let failed: Vec<&str> = checks.iter().filter(|c| !c.ok).map(|c| c.id.as_str()).collect();
    log(s, &l, "status", json!({"status": status.id(), "failed": failed}));
    sync_claims(s);
    let l = link_at(s, i, C)?;
    let mut out = link_json(s, &l, false);
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

/// The refusals `force` overrides for a swap: a stale source, a status other than passed.
fn swap_checks(s: &mut Session, l: &ShadowLink, need_passed: bool) -> (Vec<String>, Option<String>) {
    let mut why = vec![];
    let p = s.project.clone();
    let now = now_prints(&mut s.shadows, &p, l.source);
    let mut summary = None;
    if is_stale(&l.base, &now) {
        let d = diff(&l.base, &now, s.shadows.bases.get(&l.shadow), &p);
        let sum = d["summary"].as_str().unwrap_or("").to_string();
        why.push(format!("stale: the live comp changed since {} ({sum})", if l.swapped { "the swap" } else { "the shadow was made" }));
        summary = Some(sum);
    }
    if need_passed {
        let (status, _) = status_of(s, l);
        if status != ShadowStatus::Passed {
            why.push(format!("status is {} (not passed)", status.id()));
        }
    }
    (why, summary)
}

fn swap_in(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.swapIn";
    refresh(s);
    let force = b_p(p, "force").unwrap_or(false);
    let i = shadow_index(s, p, C)?;
    let l = link_at(s, i, C)?;
    if l.swapped {
        return Err(bad(C, "already swapped in (shadow.revert swaps it back)"));
    }
    let (why, _) = swap_checks(s, &l, true);
    if !why.is_empty() && !force {
        return Err(bad(
            C,
            format!("refused: {}. shadow.rebase to take Elias's latest version, set the status, or pass `force: true` to swap in anyway", why.join("; ")),
        ));
    }
    let lifted = lift_claims(s, l.source);
    let label = format!("Swap In ✎{}", l.agent);
    let r = s.edit(&label, None, |proj, _| swap(proj, i, C));
    if let Err(e) = r {
        if let Some(c) = lifted {
            s.claims.map.insert(l.source, c);
        }
        return Err(e);
    }
    s.sanitize_state();
    s.shadows.bases.insert(l.shadow, snapshot(&s.project, l.source));
    s.shadows.stale_seen.insert(l.shadow, false);
    log(s, &l, "swapped", json!({"forced": why}));
    sync_claims(s);
    let l = link_at(s, i, C)?;
    let mut out = link_json(s, &l, false);
    out["forced"] = json!(why);
    out["undo"] = json!(format!("edit.undo, or shadow.revert {{comp: {}}}: the shadow {} now holds the comp as it was (Before)", l.source.0, l.shadow.0));
    Ok(out)
}

fn revert(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.revert";
    refresh(s);
    let force = b_p(p, "force").unwrap_or(false);
    let i = if p.get("shadow").is_some_and(|v| !v.is_null()) {
        shadow_index(s, p, C)?
    } else {
        if p.get("comp").is_none_or(Value::is_null) {
            return Err(bad(C, "give `comp` (the live comp a shadow was swapped into) or `shadow`"));
        }
        let comp = comp_id(s, p)?;
        s.project
            .shadows
            .iter()
            .rposition(|l| l.source == comp && l.swapped)
            .ok_or_else(|| bad(C, format!("no shadow is swapped into \"{}\"", item_name(&s.project, comp))))?
    };
    let l = link_at(s, i, C)?;
    if !l.swapped {
        return Err(bad(C, "this shadow isn't swapped in (nothing to revert)"));
    }
    let (why, _) = swap_checks(s, &l, false);
    if !why.is_empty() && !force {
        return Err(bad(C, format!("refused: {}; reverting would undo those edits too. Pass `force: true` to revert anyway", why.join("; "))));
    }
    let lifted = lift_claims(s, l.source);
    let label = format!("Revert ✎{}", l.agent);
    let r = s.edit(&label, None, |proj, _| swap(proj, i, C));
    if let Err(e) = r {
        if let Some(c) = lifted {
            s.claims.map.insert(l.source, c);
        }
        return Err(e);
    }
    s.sanitize_state();
    s.shadows.bases.insert(l.shadow, snapshot(&s.project, l.source));
    s.shadows.stale_seen.insert(l.shadow, false);
    log(s, &l, "reverted", json!({"forced": why}));
    sync_claims(s);
    let l = link_at(s, i, C)?;
    let mut out = link_json(s, &l, false);
    out["forced"] = json!(why);
    Ok(out)
}

fn rebase(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.rebase";
    refresh(s);
    let i = shadow_index(s, p, C)?;
    let l = link_at(s, i, C)?;
    if l.swapped {
        return Err(bad(C, "this shadow is swapped in: there is nothing to rebase (make a new shadow for more work)"));
    }
    if s.project.comp(l.source).is_none() {
        return Err(bad(C, "the live comp is gone"));
    }
    let pr = s.project.clone();
    let now = now_prints(&mut s.shadows, &pr, l.source);
    let changes = diff(&l.base, &now, s.shadows.bases.get(&l.shadow), &pr);
    let ids = graph(&s.project, l.source).map_err(|m| bad(C, m))?;
    let revision = s.revision;
    let label = format!("Rebase Shadow ✎{}", l.agent);
    s.edit(&label, None, |proj, _| {
        let folder = match l.folder.filter(|f| proj.item(*f).is_some_and(|i| i.is_folder())) {
            Some(f) => f,
            None => folder_for(proj),
        };
        // The root copy keeps its id (the shadow's id): reuse every copy that still exists.
        let pairs = copy_graph(proj, &ids, &l.agent, folder, &l.copies)?;
        let kept: BTreeSet<ItemId> = pairs.iter().map(|(_, c)| *c).collect();
        let doomed: BTreeSet<ItemId> = l.copies.iter().map(|(_, c)| *c).filter(|c| !kept.contains(c) && proj.item(*c).is_some()).collect();
        delete_items(proj, &doomed);
        let base = prints_of(proj, &ids);
        let slot = proj.shadows.get_mut(i).ok_or_else(|| bad(C, "the shadow vanished"))?;
        slot.copies = pairs;
        slot.folder = Some(folder);
        slot.base = base;
        slot.base_revision = revision;
        slot.status = ShadowStatus::Working;
        slot.checks.clear();
        Ok(())
    })?;
    s.sanitize_state();
    s.shadows.bases.insert(l.shadow, snapshot(&s.project, l.source));
    s.shadows.notes.insert(l.shadow, Note { status: ShadowStatus::Working, checks: vec![] });
    s.shadows.stale_seen.insert(l.shadow, false);
    log(s, &l, "rebased", json!({"summary": changes["summary"]}));
    sync_claims(s);
    let l = link_at(s, i, C)?;
    let mut out = link_json(s, &l, false);
    out["elias"] = changes;
    out["note"] = json!("the shadow is a fresh copy of the live comp (your earlier shadow work is gone): redo it on top of these changes");
    Ok(out)
}

fn drop_shadow(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.drop";
    refresh(s);
    let i = shadow_index(s, p, C)?;
    let l = link_at(s, i, C)?;
    let before = link_json(s, &l, false);
    let label = format!("Drop Shadow ✎{}", l.agent);
    let removed = s.edit(&label, None, |proj, st| {
        let lives: BTreeSet<ItemId> = l.copies.iter().map(|(a, _)| *a).collect();
        let mut doomed: BTreeSet<ItemId> = l.copies.iter().map(|(_, c)| *c).filter(|c| proj.item(*c).is_some()).collect();
        // Comps the agent made inside the shadow (in the Shadows folder, used by nothing else).
        if proj.comp(l.shadow).is_some() {
            let reach = graph_stopping(proj, l.shadow, &lives).unwrap_or_default();
            for x in reach {
                if doomed.contains(&x) || l.folder.is_none() || proj.item(x).map(|i| i.parent) != Some(l.folder) {
                    continue;
                }
                let used_elsewhere = proj.item_users(x).iter().any(|(u, _)| !doomed.contains(u) && !reach_contains(proj, l.shadow, &lives, *u));
                if !used_elsewhere {
                    doomed.insert(x);
                }
            }
        }
        delete_items(proj, &doomed);
        proj.shadows.retain(|x| x.shadow != l.shadow);
        // The folder goes with the last shadow if nothing else is in it.
        if let Some(f) = l.folder
            && proj.shadows.iter().all(|x| x.folder != Some(f))
            && proj.item(f).is_some_and(|i| i.is_folder() && i.name == FOLDER_NAME)
            && !proj.items.values().any(|i| i.parent == Some(f))
        {
            proj.items.remove(&f);
            doomed.insert(f);
        }
        st.project_selection.retain(|x| !doomed.contains(x));
        Ok(doomed)
    })?;
    s.sanitize_state();
    s.shadows.notes.remove(&l.shadow);
    s.shadows.bases.remove(&l.shadow);
    s.shadows.stale_seen.remove(&l.shadow);
    let mut tomb = before;
    tomb["status"] = json!("dropped");
    tomb["dropped"] = json!(true);
    s.shadows.dropped.retain(|(d, _)| *d != l.shadow);
    s.shadows.dropped.push_back((l.shadow, tomb.clone()));
    while s.shadows.dropped.len() > MAX_DROPPED {
        s.shadows.dropped.pop_front();
    }
    log(s, &l, "dropped", json!({}));
    sync_claims(s);
    tomb["removed"] = json!(removed.iter().map(|x| x.0).collect::<Vec<_>>());
    if l.swapped {
        tomb["note"] = json!("it was swapped in: the live comp keeps the shadow's contents; the Before copy went with the shadow (undo brings it back)");
    }
    Ok(tomb)
}

fn reach_contains(p: &Project, root: ItemId, stop: &BTreeSet<ItemId>, x: ItemId) -> bool {
    graph_stopping(p, root, stop).unwrap_or_default().contains(&x)
}

fn events(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.events";
    refresh(s);
    let since = match p.get("since").filter(|v| !v.is_null()) {
        None => 0,
        Some(v) => v.as_u64().ok_or_else(|| bad(C, "`since` must be an event number (the `next` of the last reply)"))?,
    };
    let limit = match p.get("limit").filter(|v| !v.is_null()) {
        None => MAX_EVENTS,
        Some(v) => v.as_u64().filter(|n| *n > 0).ok_or_else(|| bad(C, "`limit` must be a positive whole number"))?.min(MAX_EVENTS as u64) as usize,
    };
    let list: Vec<Value> = s.shadows.events.iter().filter(|e| e["seq"].as_u64().is_some_and(|q| q > since)).take(limit).cloned().collect();
    let next = list.last().and_then(|e| e["seq"].as_u64()).unwrap_or_else(|| since.min(s.shadows.next_seq));
    Ok(json!({"events": list, "next": next, "latest": s.shadows.next_seq, "revision": s.revision}))
}

/// Layers that make sound in `comp` (directly or through precomps), seconds of the comp.
fn audio_layers(p: &Project, comp: ItemId) -> Vec<Value> {
    fn sounds(p: &Project, item: ItemId, depth: usize) -> bool {
        if depth > MAX_DEPTH {
            return false;
        }
        match p.item(item).map(|i| &i.kind) {
            Some(ItemKind::Footage(f)) => f.has_audio,
            Some(ItemKind::Comp(c)) => c.layers.iter().any(|l| l.switches.audio && l.source.item().is_some_and(|i| sounds(p, i, depth + 1))),
            _ => false,
        }
    }
    let Some(c) = p.comp(comp) else { return vec![] };
    c.layers
        .iter()
        .filter(|l| l.switches.audio && l.source.item().is_some_and(|i| sounds(p, i, 1)))
        .map(|l| json!({"layer": l.id.0, "name": l.name, "in": l.in_point.seconds(), "out": l.out_point.seconds(), "source": l.source.item().map(|i| i.0)}))
        .collect()
}

fn frames(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "shadow.frames";
    refresh(s);
    let i = if p.get("shadow").is_some_and(|v| !v.is_null()) {
        shadow_index(s, p, C)?
    } else {
        if p.get("comp").is_none_or(Value::is_null) {
            return Err(bad(C, "give `shadow`, or `comp` (a live comp that has a shadow)"));
        }
        let comp = comp_id(s, p)?;
        // The swapped one first (its Before/After is what the popup shows), else the newest.
        let pos = s
            .project
            .shadows
            .iter()
            .rposition(|l| l.source == comp && l.swapped)
            .or_else(|| s.project.shadows.iter().rposition(|l| l.source == comp || l.owns(comp)));
        pos.ok_or_else(|| bad(C, format!("\"{}\" has no shadow", item_name(&s.project, comp))))?
    };
    let l = link_at(s, i, C)?;
    let which = str_p(p, "which").unwrap_or("after").trim().to_ascii_lowercase();
    let (before, after) = if l.swapped { (l.shadow, l.source) } else { (l.source, l.shadow) };
    let target = match which.as_str() {
        "before" => before,
        "after" => after,
        other => return Err(bad(C, format!("`which` must be before or after, not {other:?}"))),
    };
    let comp = s.project.comp(target).ok_or_else(|| bad(C, "that comp is gone"))?;
    let rate = comp.frame_rate;
    let (wa_start, wa_dur) = comp.work_area;
    let duration = comp.duration;
    let max_side = match p.get("max_side").or_else(|| p.get("maxSide")).filter(|v| !v.is_null()) {
        None => DEFAULT_FRAME_SIDE,
        Some(v) => v.as_u64().filter(|n| (16..=8192).contains(n)).ok_or_else(|| bad(C, "`max_side` must be a whole number of pixels from 16 to 8192"))?,
    };
    let end = Tick(wa_start.0.saturating_add(wa_dur.0)).min(duration);
    let last = Tick(end.0.saturating_sub(rate.frame_duration().0.max(1))).max(wa_start);
    let times: Vec<f64> = match p.get("times").filter(|v| !v.is_null()) {
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| bad(C, "`times` must be a list of seconds"))?;
            if arr.len() > MAX_FRAMES {
                return Err(bad(C, format!("at most {MAX_FRAMES} times")));
            }
            let mut out = vec![];
            for t in arr {
                let t = t.as_f64().filter(|t| t.is_finite()).ok_or_else(|| bad(C, format!("`times`: {t} is not a number of seconds")))?;
                out.push(t.clamp(0.0, duration.seconds().max(0.0)));
            }
            out
        }
        None => {
            let (a, b) = (wa_start.seconds(), last.seconds());
            if b > a { vec![a, (a + b) / 2.0, b] } else { vec![a] }
        }
    };
    let base64 = b_p(p, "base64").unwrap_or(cfg!(target_arch = "wasm32"));
    let mut out_frames = vec![];
    for (k, t) in times.iter().enumerate() {
        let mut rp = json!({"comp": target.0, "time": t, "max_side": max_side});
        if base64 {
            rp["base64"] = json!(true);
        } else {
            let path = std::env::temp_dir().join(format!("effectcraft-shadow-{}-{which}-{k}-{}.png", l.shadow.0, std::process::id()));
            rp["path"] = json!(path.to_string_lossy());
        }
        let mut f = crate::render_view::render_frame_reply(s, &rp, false)?;
        f["time"] = json!(t);
        out_frames.push(f);
    }
    let audio = audio_layers(&s.project, target);
    Ok(json!({
        "shadow": l.shadow.0,
        "which": which,
        "comp": target.0,
        "compName": item_name(&s.project, target),
        "swapped": l.swapped,
        "frames": out_frames,
        "audio": {
            "hasAudio": !audio.is_empty(),
            "start": wa_start.seconds(),
            "end": end.seconds(),
            "duration": duration.seconds(),
            "layers": audio,
        },
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "shadow.create",
            "Create Shadow",
            [],
            None,
            "{comp? (default: the open comp), agent? (default: your `client`)} — copy the comp and every comp nested in it into the Shadows folder (`S1 06 ✎A2`; footage shared) for an agent to work on; its source counts as claimed by the agent meanwhile; one undo step → {shadow, name, comp, agent, status: working, copies: [{live, copy}], …}",
            super::always,
            create
        ),
        query!(
            "shadow.list",
            "List Shadows",
            "{comp? (live comp or shadow), agent?} → {shadows: [{shadow, name, comp, compName, agent, status: working|checking|passed|failed|swapped, checks: [{id, ok, note}], stale, changes?: {summary, comps}, claimedBy, before, after}], dropped: [...]}",
            list
        ),
        query!(
            "shadow.get",
            "Get Shadow",
            "{shadow: id|name} → the shadow as shadow.list gives it, plus changes (what changed in the live comp since the copy: comps, layers added/removed/changed, the properties) and copies (live → copy); a shadow dropped this session answers with status dropped",
            get
        ),
        CommandSpec {
            journal: true,
            ..query!(
                "shadow.setStatus",
                "Set Shadow Status",
                "{shadow: id|name, status: working|checking|passed|failed, checks?: [{id, ok, note?}] (replaces the recorded checks)} — record the review (no undo step: bookkeeping, kept in the project) → the shadow",
                set_status
            )
        },
        cmd!(
            "shadow.swapIn",
            "Swap In Shadow",
            [],
            None,
            "{shadow: id|name, force?: bool (swap a stale or not-passed shadow anyway)} — the live comp (and its nested comps) take the shadow's contents, keeping their ids, names and uses, as one undo step; the shadow then holds the comp as it was (Before) → the shadow with status swapped, forced: [reasons overridden]",
            super::always,
            swap_in
        ),
        cmd!(
            "shadow.revert",
            "Revert Shadow Swap",
            [],
            None,
            "{comp?: the live comp, shadow?: id|name (instead of comp), force?: bool (also when the live comp was edited after the swap)} — swap the Before back in, one undo step → the shadow (status as before the swap)",
            super::always,
            revert
        ),
        cmd!(
            "shadow.rebase",
            "Rebase Shadow",
            [],
            None,
            "{shadow: id|name} — copy the live comp's latest version into the shadow again (same shadow id; the agent's earlier shadow work is replaced) → the shadow, elias: {changed, summary, comps: [{comp, name, settings, added, removed, changed: [{layer, name, parts, props?}]}]} (what the user changed since the last copy)",
            super::always,
            rebase
        ),
        cmd!(
            "shadow.drop",
            "Drop Shadow",
            [],
            None,
            "{shadow: id|name} — delete the shadow, its copies and the comps made only inside it (one undo step) → {shadow, status: dropped, removed: [item ids]}",
            super::always,
            drop_shadow
        ),
        query!(
            "shadow.events",
            "Shadow Events",
            "{since? (event number: the `next` of the last reply), limit?} → {events: [{seq, kind: created|status|stale|fresh|swapped|reverted|rebased|dropped, shadow, name, comp, compName, agent, summary?, message?, revision}], next, latest, revision}: stale = the user edited a comp an agent works on (warn him; the agent rebases)",
            events
        ),
        query!(
            "shadow.frames",
            "Shadow Before/After Frames",
            "{shadow?: id|name, comp?: a live comp with a shadow (instead of shadow), which?: before|after (default after), times?: [s] (default: start, middle and end of the work area), max_side? (default 480), base64?} → {shadow, which, comp, compName, swapped, frames: [{time, path|png, width, height, …}], audio: {hasAudio, start, end, duration, layers: [{layer, name, in, out, source}]}}",
            frames
        ),
    ]
}
