//! Undo your own step when others worked after it.
//!
//! With agents and the user in one project, a plain Undo reverts whoever made the last step:
//! the user's ⌘Z would silently remove an agent's work, and after a few agent steps the user's
//! own history would be out of reach. `edit.undo` therefore undoes the caller's own latest step
//! (the user's, or a `client`'s): when someone else made the last step, it reverts only what the
//! caller's step changed, as a new step, and only where nobody changed the same thing since.
//! Otherwise it refuses, naming who did, and changes nothing.
//!
//! What a step changed is compared per project item (a composition, a footage item, a folder)
//! and for the project's own settings, and inside a composition per layer: an agent working on
//! one layer doesn't block undoing the user's change to another layer of the same comp. Comps and
//! their layers are shared between states, so comparing them is cheap.

use std::collections::BTreeSet;
use std::sync::Arc;

use effectcraft_project::{Comp, Item, ItemId, ItemKind, Layer, LayerId, Layers, Project};

use crate::{EngineError, Result, Session};

/// Whether two versions of an item are the same (a shared composition compares by pointer).
fn item_eq(a: Option<&Item>, b: Option<&Item>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let kind = match (&a.kind, &b.kind) {
                (ItemKind::Comp(x), ItemKind::Comp(y)) => Arc::ptr_eq(x, y) || x == y,
                (x, y) => x == y,
            };
            kind && meta_eq(a, b)
        }
        _ => false,
    }
}

/// Whether two items have the same name, label, comment, folder and proxy (everything but their
/// contents).
fn meta_eq(a: &Item, b: &Item) -> bool {
    a.id == b.id && a.name == b.name && a.label == b.label && a.comment == b.comment && a.parent == b.parent && a.proxy == b.proxy
}

fn layer_eq(a: Option<&Arc<Layer>>, b: Option<&Arc<Layer>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
        _ => false,
    }
}

fn find(c: &Comp, id: LayerId) -> Option<&Arc<Layer>> {
    c.layers.arcs().iter().find(|l| l.id == id)
}

/// Layer ids in stack order.
fn order(c: &Comp) -> Vec<LayerId> {
    c.layers.iter().map(|l| l.id).collect()
}

/// A comp's settings (everything but its layers).
fn settings(c: &Comp) -> Comp {
    Comp { layers: Layers::new(), ..c.clone() }
}

/// Revert a step's change to a composition (`before` → `after`) in its current version `cur`,
/// which others changed since, layer by layer. `Err`: what both the step and someone after it
/// changed (layers, the comp's settings or name, or the layer order).
fn merge_comp(before: &Item, after: &Item, cur: &Item) -> std::result::Result<Item, Vec<String>> {
    let (ItemKind::Comp(b), ItemKind::Comp(a), ItemKind::Comp(c)) = (&before.kind, &after.kind, &cur.kind) else {
        return Err(vec![format!("“{}”", after.name)]);
    };
    let mut clashes = vec![];
    let meta = if meta_eq(before, after) {
        cur
    } else if meta_eq(cur, after) {
        before
    } else {
        clashes.push(format!("the name or folder of “{}”", after.name));
        cur
    };
    let mut comp = if settings(b) == settings(a) {
        (**c).clone()
    } else if settings(c) == settings(a) {
        Comp { layers: c.layers.clone(), ..(**b).clone() }
    } else {
        clashes.push(format!("the settings of “{}”", after.name));
        (**c).clone()
    };
    let ids: BTreeSet<LayerId> = order(b).into_iter().chain(order(a)).collect();
    let changed: Vec<LayerId> = ids.into_iter().filter(|&id| !layer_eq(find(b, id), find(a, id))).collect();
    for &id in &changed {
        if !layer_eq(find(c, id), find(a, id)) {
            let name = find(c, id).or(find(a, id)).or(find(b, id)).map(|l| l.name.clone()).unwrap_or_default();
            clashes.push(format!("“{name}” in “{}”", after.name));
        }
    }
    // Did the step move layers (not just add or delete them)?
    let common = |x: &Comp, y: &Comp| order(x).into_iter().filter(|id| find(y, *id).is_some()).collect::<Vec<_>>();
    let moved = common(b, a) != common(a, b);
    if moved && order(c) != order(a) {
        clashes.push(format!("the layer order of “{}”", after.name));
    }
    if !clashes.is_empty() {
        return Err(clashes);
    }
    let layers: Vec<Arc<Layer>> = if moved {
        // Nobody added, deleted or moved layers since: the step's order, with others' changes.
        order(b).into_iter().filter_map(|id| if changed.contains(&id) { find(b, id) } else { find(c, id) }.cloned()).collect()
    } else {
        // Revert the step's layers where they are now; drop the ones it added and put back the
        // ones it deleted (above the layer that followed them).
        let mut out: Vec<Arc<Layer>> = c
            .layers
            .arcs()
            .iter()
            .filter(|l| !changed.contains(&l.id) || find(b, l.id).is_some())
            .map(|l| if changed.contains(&l.id) { find(b, l.id).cloned().unwrap_or_else(|| l.clone()) } else { l.clone() })
            .collect();
        let was = b.layers.arcs();
        for (i, l) in was.iter().enumerate().rev() {
            if find(a, l.id).is_some() {
                continue;
            }
            let at = was.get(i + 1..).and_then(|below| below.iter().find_map(|n| out.iter().position(|o| o.id == n.id))).unwrap_or(out.len());
            out.insert(at.min(out.len()), l.clone());
        }
        out
    };
    comp.layers = layers.into_iter().collect();
    Ok(Item { kind: ItemKind::Comp(Arc::new(comp)), ..meta.clone() })
}

/// The project without its items and id counter (its settings, render queue, …).
fn rest(p: &Project) -> Project {
    let mut q = Project { items: Default::default(), ..p.clone() };
    q.next_id = 0;
    q
}

fn who(author: Option<&str>) -> String {
    author.map_or_else(|| "the user".to_string(), |a| format!("client `{a}`"))
}

/// What taking back the step `before` → `after` makes of the current project `cur`: each item the
/// step changed as it was before it, where nobody changed it since (a composition layer by
/// layer). `Err`: what both the step and someone after it changed.
fn revert(before: &Project, after: &Project, cur: &Project) -> std::result::Result<Project, Vec<String>> {
    let mut items = cur.items.clone();
    let mut clashes: Vec<String> = vec![];
    let ids: BTreeSet<ItemId> = before.items.keys().chain(after.items.keys()).copied().collect();
    for id in ids {
        let (b, a) = (before.items.get(&id), after.items.get(&id));
        if item_eq(b, a) {
            continue;
        }
        if !item_eq(cur.items.get(&id), a) {
            // Someone changed it since: a comp may still be undone layer by layer.
            match (b, a, cur.items.get(&id)) {
                (Some(b), Some(a), Some(c)) => match merge_comp(b, a, c) {
                    Ok(item) => {
                        items.insert(id, item);
                    }
                    Err(c) => clashes.extend(c),
                },
                _ => clashes.push(format!("“{}”", a.or(b).map(|i| i.name.clone()).unwrap_or_default())),
            }
            continue;
        }
        match b {
            Some(b) => items.insert(id, b.clone()),
            None => items.remove(&id),
        };
    }
    let settings = rest(before) != rest(after);
    if settings && rest(cur) != rest(after) {
        clashes.push("the project settings".into());
    }
    if !clashes.is_empty() {
        return Err(clashes);
    }
    let mut next = if settings { before.clone() } else { cur.clone() };
    next.items = items;
    next.next_id = cur.next_id;
    Ok(next)
}

/// Undo `me`'s (`None`: the user's) latest step on the undo line as a new step. `Ok(None)`: the
/// line holds no step of theirs. Refused, changing nothing, when someone changed the same item
/// (or the project settings) after it.
pub fn undo_own(s: &mut Session, me: Option<&str>) -> Result<Option<String>> {
    let n = s.history.undo.len();
    // State k + 1 is what step k made (the last one made the current state).
    let made: Vec<Arc<Project>> = (0..n).map(|k| s.history.undo.get(k + 1).map_or_else(|| s.project.clone(), |(_, p)| p.clone())).collect();
    let Some(k) = (0..n).rev().find(|&k| made.get(k).is_some_and(|after| s.history.author(after) == me && !s.history.settled(after))) else { return Ok(None) };
    let (Some((label, before)), Some(after)) = (s.history.undo.get(k).cloned(), made.get(k).cloned()) else { return Ok(None) };
    let cur = s.project.clone();
    let later: Vec<(String, Option<String>)> =
        (k + 1..n).filter_map(|j| Some((s.history.undo.get(j)?.0.clone(), s.history.author(made.get(j)?).map(str::to_string)))).collect();

    let next = match revert(&before, &after, &cur) {
        Ok(next) => next,
        Err(clashes) => {
            let (last, last_by) = later.last().cloned().unwrap_or_default();
            let mine = me.map_or_else(|| "you".to_string(), |m| format!("`{m}`"));
            let others: BTreeSet<String> = later.iter().filter(|(_, a)| a.as_deref() != me).map(|(_, a)| who(a.as_deref())).collect();
            let way = if me.is_some() {
                "undo it there, or undo without `client`"
            } else {
                "use the History panel, or edit.undo {any: true} to undo the last change whoever made it"
            };
            return Err(EngineError::Other(format!(
                "the last change (“{last}”) was made by {}, not {mine}, and {} changed {} since your “{label}”; {way}",
                who(last_by.as_deref()),
                others.into_iter().collect::<Vec<_>>().join(", "),
                clashes.join(", "),
            )));
        }
    };
    s.edit(&format!("Undo {label}"), None, move |p, _| {
        *p = next;
        Ok(())
    })?;
    // Undone, and the undo itself isn't a step to undo next: ⌘Z again goes on to the one before.
    s.history.settle(&after);
    let undo_step = s.project.clone();
    s.history.settle(&undo_step);
    s.history.set_origin(&undo_step, &after);
    s.sanitize_state();
    Ok(Some(label))
}

/// One step [`undo_changes`] took back.
#[derive(Clone, Debug, PartialEq)]
pub struct Undone {
    /// The feed revision of the step taken back.
    pub revision: u64,
    /// The new step's label: "Undo <label>", or "Redo <label>" for a step that was "Undo <label>".
    pub label: String,
    /// The revision the new step made.
    pub step: u64,
}

/// Take back the steps the feed lists at `revisions`, any age and whoever made them, newest first,
/// each as a new step ("Undo <its label>"; taking back "Undo X" is "Redo X"). All or nothing: when
/// a later change touched the same item (a layer, a comp's settings, the layer order, the project
/// settings) as one of them, nothing changes and the error names what.
///
/// CDXC:Editing 2026-10-31 WHY: Ghostex's per-change Undo (EDITING's edit list) could only plain-undo
/// a chat's newest step; an older change went to that chat's Claude as a prompt, which was slow and
/// not deterministic. This reverts exactly what the step changed, with the same per-layer check
/// `undo_own` uses, so a clash is refused instead of guessed at; the steps it takes back (and the
/// new ones) are settled like `undo_own`'s, and taking back such an undo unsettles the original, so
/// a client's `edit.undo` afterwards still picks its right step.
pub fn undo_changes(s: &mut Session, revisions: &[u64]) -> Result<Vec<Undone>> {
    let mut wanted: Vec<u64> = revisions.to_vec();
    wanted.sort_unstable_by(|a, b| b.cmp(a));
    wanted.dedup();
    if wanted.is_empty() {
        return Err(EngineError::Other("no revisions given".into()));
    }
    let mut steps: Vec<(u64, String, Arc<Project>, Arc<Project>)> = vec![];
    for &revision in &wanted {
        let Some(entry) = s.feed.since(revision.saturating_sub(1)).find(|e| e.revision == revision) else {
            return Err(EngineError::Other(format!(
                "revision {revision} isn't in the change feed (it keeps the last changes of this project only); undo it in the History panel"
            )));
        };
        let (before, after) = entry.projects();
        steps.push((revision, entry.label.clone(), before.clone(), after.clone()));
    }
    // A dry run first: nothing changes unless every step can be taken back.
    let mut cur = s.project.clone();
    let mut refused: Vec<String> = vec![];
    for (_, label, before, after) in &steps {
        match revert(before, after, &cur) {
            Ok(next) => cur = Arc::new(next),
            Err(clashes) => refused.push(format!("“{label}” ({})", clashes.join(", "))),
        }
    }
    if !refused.is_empty() {
        return Err(EngineError::Other(format!(
            "a later change touched the same thing, so it can't be undone alone: {}; undo the later change first",
            refused.join("; ")
        )));
    }
    let mut done = vec![];
    for (revision, label, before, after) in steps {
        let next = revert(&before, &after, &s.project).map_err(|clashes| EngineError::Other(format!("“{label}” changed meanwhile: {}", clashes.join(", "))))?;
        let (new_label, redo) = match label.strip_prefix("Undo ") {
            Some(was) => (format!("Redo {was}"), true),
            None => (format!("Undo {}", label.strip_prefix("Redo ").unwrap_or(&label)), false),
        };
        s.edit(&new_label, None, move |p, _| {
            *p = next;
            Ok(())
        })?;
        let origin = s.history.origin(&after).unwrap_or_else(|| after.clone());
        if redo {
            s.history.unsettle(&origin);
        } else {
            s.history.settle(&origin);
        }
        s.history.settle(&after);
        let made = s.project.clone();
        s.history.settle(&made);
        s.history.set_origin(&made, &origin);
        done.push(Undone { revision, label: new_label, step: s.revision });
    }
    s.sanitize_state();
    Ok(done)
}

/// The step `edit.undo` would undo for `me` (`None`: the user): the last step when `me` made it,
/// else `me`'s latest step (the Edit menu's "Undo <step>"). `None`: nothing to undo.
pub fn own_undo_label(s: &Session, me: Option<&str>) -> Option<String> {
    let n = s.history.undo.len();
    let made = |k: usize| s.history.undo.get(k + 1).map_or_else(|| s.project.clone(), |(_, p)| p.clone());
    let own = (0..n).rev().find(|&k| {
        let after = made(k);
        s.history.author(&after) == me && !s.history.settled(&after)
    });
    // Nothing left but own-step undos: the last one is undone plainly (it brings its step back).
    let k = own.or_else(|| n.checked_sub(1).filter(|_| s.history.author(&s.project) == me))?;
    s.history.undo.get(k).map(|(l, _)| l.clone())
}
