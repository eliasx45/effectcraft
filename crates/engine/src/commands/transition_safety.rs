//! Elias's safety rules for `transition.apply` (5 Nov 2026: "they gotta be careful"), for every
//! family (zoom, FX, scene cuts):
//!
//! 1. Never cut the sound. A kind that trims a shot (an early end of the old shot, a later start of
//!    the new one, the old shot playing past the cut) trims only its picture: when the shot has
//!    audio, an audio-only copy of it (video off, the same source time) plays over exactly the span
//!    the trim took, right below its shot, named `<shot> audio (<Family> keep)` and tagged for the
//!    pair. A kind that starts the new shot's picture early (under or over the old one) never starts
//!    its sound early: one copy carries the shot's whole span as it was and the shot's own audio goes
//!    off. Re-applying or switching kinds deletes the copies and gives the shot its span (and its
//!    audio) back. The copies of
//!    a shot a kind shows in its `… Scenes` precomps are muted, so the sound never doubles.
//! 2. Never touch a third layer. A restack (an FX cut's old-over-new, a scene cut's new-over-old)
//!    that would move a shot past another layer, a kind that composites the two shots together while
//!    another layer sits between them, and a transition whose window would overlap another
//!    transition's window on the same shot are errors that name what is in the way (the whole apply
//!    rolls back). Re-applying on a pair removes only that pair's effects from a shot it shares with
//!    another transition.
//! 3. Report exactly what moved: the reply's `moved` (see [`super::transition`]) lists the audio
//!    copies too (`audio-keep`), with comp frames as well as seconds.

use effectcraft_project::{Comp, ItemId, Layer, LayerId, LayerSource, Node, PropGroup, Uid};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::bad;
use super::transition::{C, arr, fmt_s};
use crate::{EngineError, Result, Session};

/// The part of a layer comment that marks an audio keep copy: `<pair tag> · audio keep <shot id>`.
pub(super) const AUDIO_KEEP: &str = " · audio keep ";

/// The three families' name prefixes.
pub(super) const PREFIXES: [&str; 3] = [super::transition::PREFIX, super::transition_fx::PREFIX, super::transition_scene::PREFIX];

/// Which end of a shot a transition's effect belongs to: its in point (the shot is the new one of
/// that pair) or its out point (the old one).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Side {
    In,
    Out,
}

fn prefix_of(name: &str) -> Option<&'static str> {
    PREFIXES.iter().copied().find(|p| name.starts_with(p))
}

/// The table a family prefix reads.
fn table_of(prefix: &str) -> Option<&'static Value> {
    if prefix == super::transition::PREFIX {
        super::transition::table().ok()
    } else if prefix == super::transition_fx::PREFIX {
        super::transition_fx::table().ok()
    } else {
        super::transition_scene::table().ok()
    }
}

/// Comp times of a group's keys, earliest and latest.
fn key_span(l: &Layer, g: &PropGroup) -> Option<(Tick, Tick)> {
    let mut span: Option<(Tick, Tick)> = None;
    g.walk("", &mut |_, pr| {
        for k in &pr.keys {
            let t = l.comp_time(k.time);
            span = Some(match span {
                None => (t, t),
                Some((a, b)) => (a.min(t), b.max(t)),
            });
        }
    });
    span
}

/// A transition effect's side on its layer: by the kind's table when only one role has an effect
/// of that name (keyed or keyless like this one), else by its keys (an in-side move starts at the
/// in point, an out-side one ends at the out point; a tie is `None`).
fn side_alone(l: &Layer, g: &PropGroup) -> Option<Side> {
    let prefix = prefix_of(&g.name)?;
    let keys = key_span(l, g);
    let by_table = (|| {
        let rest = g.name.strip_prefix(prefix)?;
        let (kind, ename) = rest.split_once(' ')?;
        let k = table_of(prefix)?.get("kinds")?.get(kind)?;
        let keyed = |e: &Value| e.get("keys").and_then(Value::as_object).is_some_and(|m| !m.is_empty());
        let roles: Vec<Side> = [("old", Side::Out), ("new", Side::In)]
            .into_iter()
            .filter(|(role, _)| {
                k.get("layers")
                    .and_then(|ls| ls.get(*role))
                    .is_some_and(|sp| arr(sp, "effects").iter().any(|e| e.get("name").and_then(Value::as_str) == Some(ename) && keyed(e) == keys.is_some()))
            })
            .map(|(_, s)| s)
            .collect();
        match roles[..] {
            [one] => Some(one),
            _ => None,
        }
    })();
    if by_table.is_some() {
        return by_table;
    }
    let (a, b) = keys?;
    let d_in = (a.seconds() - l.in_point.seconds()).abs();
    let d_out = (b.seconds() - l.out_point.seconds()).abs();
    if d_in < d_out {
        Some(Side::In)
    } else if d_out < d_in {
        Some(Side::Out)
    } else {
        None
    }
}

/// Every transition effect on a layer with its side (one it can't tell takes the side of the same
/// kind's other effects on the layer, when they agree).
pub(super) fn sides(l: &Layer) -> Vec<(Uid, String, Option<Side>)> {
    let Some(fx) = l.effects() else { return vec![] };
    let mine: Vec<(Uid, String, Option<Side>)> =
        fx.groups().filter(|g| prefix_of(&g.name).is_some()).map(|g| (g.uid, g.name.clone(), side_alone(l, g))).collect();
    let kind_key = |n: &str| -> String {
        let p = prefix_of(n).unwrap_or("");
        let k = n.strip_prefix(p).and_then(|r| r.split(' ').next()).unwrap_or("");
        format!("{p}{k}")
    };
    mine.iter()
        .map(|(uid, name, side)| {
            let side = side.or_else(|| {
                let kk = kind_key(name);
                let mut found = mine.iter().filter(|(_, n, s)| s.is_some() && kind_key(n) == kk).filter_map(|x| x.2);
                let first = found.next()?;
                found.all(|s| s == first).then_some(first)
            });
            (*uid, name.clone(), side)
        })
        .collect()
}

/// The transition effects on `l` that belong to its pair's `side` (an effect whose side can't be
/// told is counted as the pair's: the behaviour before shared shots were looked after).
pub(super) fn owned(l: &Layer, side: Side) -> Vec<Uid> {
    sides(l).into_iter().filter(|(_, _, s)| s.is_none_or(|s| s == side)).map(|x| x.0).collect()
}

/// A `<Prefix><old>-><new>` tag at the start of a comment: (prefix, old, new).
fn parse_tag(comment: &str) -> Option<(&'static str, LayerId, LayerId)> {
    let p = prefix_of(comment)?;
    let rest = comment.strip_prefix(p)?;
    let head = rest.split(' ').next()?;
    let (a, b) = head.split_once("->")?;
    Some((p, LayerId(a.parse().ok()?), LayerId(b.parse().ok()?)))
}

/// An audio keep copy's shot (its comment's `· audio keep <id>`).
pub(super) fn keep_of(comment: &str) -> Option<LayerId> {
    let (_, rest) = comment.split_once(AUDIO_KEEP)?;
    rest.split(' ').next()?.parse().ok().map(LayerId)
}

/// Another transition on one of the pair's shots: what it is and the comp frames it acts in.
pub(super) struct Other {
    shot: LayerId,
    what: String,
    a: i64,
    b: i64,
}

/// The other transitions on the pair's shots (layers tagged for another pair with one of the shots;
/// effects on the shots for their other end), as windows of comp frames.
pub(super) fn others(s: &Session, cid: ItemId, old: LayerId, new: LayerId) -> Vec<Other> {
    let Some(c) = s.project.comp(cid) else { return vec![] };
    let fr = c.frame_rate;
    let frame = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    let name = |id: LayerId| c.layer(id).map_or_else(|| format!("layer {}", id.0), |l| l.name.clone());
    let mut out: Vec<Other> = vec![];
    let mut add = |shot: LayerId, what: String, a: i64, b: i64| match out.iter_mut().find(|o| o.shot == shot && o.what == what) {
        Some(o) => (o.a, o.b) = (o.a.min(a), o.b.max(b)),
        None => out.push(Other { shot, what, a, b }),
    };
    for l in c.layers.iter() {
        let Some((p, x, y)) = parse_tag(&l.comment) else { continue };
        // A keep copy plays the other pair's sound, it isn't its window.
        if (x, y) == (old, new) || l.id == old || l.id == new || keep_of(&l.comment).is_some() {
            continue;
        }
        for shot in [old, new] {
            if x == shot || y == shot {
                let what = format!("the {}cut between “{}” and “{}”", family_word(p), name(x), name(y));
                add(shot, what, frame(l.in_point), frame(l.out_point));
            }
        }
    }
    for (shot, theirs) in [(old, Side::In), (new, Side::Out)] {
        let Some(l) = c.layer(shot) else { continue };
        for (uid, ename, side) in sides(l) {
            if side != Some(theirs) {
                continue;
            }
            let Some(g) = l.effects().and_then(|fx| fx.groups().find(|g| g.uid == uid)) else { continue };
            let Some((a, b)) = key_span(l, g) else { continue };
            let end = if theirs == Side::In { "in" } else { "out" };
            let p = prefix_of(&ename).unwrap_or("");
            let kind = ename.strip_prefix(p).and_then(|r| r.split(' ').next()).unwrap_or("");
            add(shot, format!("the {}cut {kind} at its {end} point", family_word(p)), frame(a), frame(b));
        }
    }
    out
}

fn family_word(prefix: &str) -> &'static str {
    match prefix {
        p if p == super::transition::PREFIX => "zoom ",
        p if p == super::transition_fx::PREFIX => "FX ",
        _ => "scene ",
    }
}

/// Rule 2: this transition's window (comp frames `lo`–`hi`) may not overlap another transition's
/// window on the same shot.
pub(super) fn check_windows(s: &Session, cid: ItemId, kind: &str, span: (i64, i64), others: &[Other]) -> Result<()> {
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let fr = c.frame_rate;
    let (lo, hi) = span;
    for o in others {
        if o.a < hi && lo < o.b {
            let shot = c.layer(o.shot).map_or_else(String::new, |l| l.name.clone());
            return Err(bad(
                C,
                format!(
                    "{kind} would run {}–{} s, but “{shot}” already carries {} from {} to {} s: the two transitions would overlap on that shot. Move the cut, shorten it (`dur`), or remove the other transition first",
                    fmt_s(fr.tick_of(lo)),
                    fmt_s(fr.tick_of(hi)),
                    o.what,
                    fmt_s(fr.tick_of(o.a)),
                    fmt_s(fr.tick_of(o.b))
                ),
            ));
        }
    }
    Ok(())
}

/// The layers strictly between `a` and `b` in the stack (audio keep copies don't count: they have no
/// picture), optionally only those on during `during` (comp ticks).
pub(super) fn between(c: &Comp, a: LayerId, b: LayerId, during: Option<(Tick, Tick)>) -> Vec<String> {
    let (Some(ia), Some(ib)) = (c.index_of(a), c.index_of(b)) else { return vec![] };
    let (lo, hi) = (ia.min(ib), ia.max(ib));
    c.layers
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > lo && *i < hi)
        .map(|(_, l)| l)
        .filter(|l| keep_of(&l.comment).is_none())
        .filter(|l| during.is_none_or(|(t0, t1)| l.in_point < t1 && l.out_point > t0))
        .map(|l| format!("“{}”", l.name))
        .collect()
}

/// Rule 2: moving `moving` right above `over` would move it past `between` layers.
pub(super) fn check_restack(s: &Session, cid: ItemId, kind: &str, moving: LayerId, over: LayerId) -> Result<()> {
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mid = between(c, moving, over, None);
    if mid.is_empty() {
        return Ok(());
    }
    let n = |id: LayerId| c.layer(id).map_or_else(String::new, |l| l.name.clone());
    Err(bad(
        C,
        format!(
            "{kind} needs “{}” right above “{}”, but {} {} between them: it would move the shot past {}. Put the two shots next to each other in the stack first",
            n(moving),
            n(over),
            mid.join(", "),
            if mid.len() == 1 { "is" } else { "are" },
            if mid.len() == 1 { "it" } else { "them" }
        ),
    ))
}

/// Rule 2: a kind that composites the two shots together (they overlap in time, or it puts a layer
/// under or between them) needs nothing between them while it runs (`during`, comp ticks).
pub(super) fn check_adjacent(s: &Session, cid: ItemId, kind: &str, old: LayerId, new: LayerId, during: (Tick, Tick)) -> Result<()> {
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mid = between(c, old, new, Some(during));
    if mid.is_empty() {
        return Ok(());
    }
    let n = |id: LayerId| c.layer(id).map_or_else(String::new, |l| l.name.clone());
    Err(bad(
        C,
        format!(
            "{kind} shows “{}” and “{}” together, but {} {} between them in the stack while it runs ({}–{} s): put the two shots next to each other first",
            n(old),
            n(new),
            mid.join(", "),
            if mid.len() == 1 { "is" } else { "are" },
            fmt_s(during.0),
            fmt_s(during.1)
        ),
    ))
}

/// The comment ending of an audio keep copy that turned its shot's own audio off: the transition
/// shows the shot's picture earlier (or later) than its sound played, so the copy carries all of it.
pub(super) const MUTED: &str = " · muted";

/// Does the layer make sound (Audio switch on, footage with audio, a precomp with sound, a tone)?
pub(super) fn has_audio(s: &Session, l: &Layer) -> bool {
    effectcraft_render::audio::audible(&s.project, l)
        && match l.source {
            LayerSource::Comp { item } => effectcraft_render::audio::comp_has_audio(&s.project, item),
            _ => true,
        }
}

/// Rule 1: for each shot (`(before, now)`: its id before the transition and the layer now showing
/// it, the window for into-window) whose span the transition shortened, an audio-only copy over
/// each span it lost, right below it. A shot whose picture now starts earlier (or ends later) than
/// before (the new shot under or over the old one from the window's start) would play its sound
/// early: one copy carries its whole span as it was and the shot's own audio goes off (the shelf's
/// cut-safety `mute`), so the mix is what it was. `spans`: the shots' (in, out) before the
/// transition (after the previous one's trims went back). Returns the reply's `audio-keep` entries.
pub(super) fn keep_audio(
    s: &mut Session,
    cid: ItemId,
    shots: &[(LayerId, LayerId)],
    spans: &[(LayerId, Tick, Tick)],
    tag: &str,
    prefix: &str,
) -> Result<Vec<Value>> {
    let fr: FrameRate = s.project.comp(cid).ok_or(EngineError::NoComp)?.frame_rate;
    // (shot, from, to, mute the shot)
    let mut plan: Vec<(LayerId, Tick, Tick, bool)> = vec![];
    for (was, now) in shots {
        let Some((_, in0, out0)) = spans.iter().find(|x| x.0 == *was).copied() else { continue };
        let Some(l) = s.project.comp(cid).and_then(|c| c.layer(*now)) else { continue };
        if !has_audio(s, l) {
            continue;
        }
        if l.in_point < in0 || l.out_point > out0 {
            plan.push((*now, in0, out0, true));
            continue;
        }
        if l.in_point > in0 {
            plan.push((*now, in0, l.in_point, false));
        }
        if l.out_point < out0 {
            plan.push((*now, l.out_point, out0, false));
        }
    }
    if plan.is_empty() {
        return Ok(vec![]);
    }
    let fam = prefix.trim_end().to_string();
    let (tag, plan2) = (tag.to_string(), plan.clone());
    let made = s.edit("Keep Audio", None, move |proj, _| {
        let mut next = proj.next_id;
        let comp = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        let mut made = vec![];
        for (shot, a, b, mute) in &plan2 {
            let i = comp.layers.iter().position(|l| l.id == *shot).ok_or_else(|| bad(C, "a shot is gone"))?;
            let mut l = comp.layer(*shot).ok_or_else(|| bad(C, "a shot is gone"))?.clone();
            super::edit::reid(&mut l, &mut next);
            l.name = format!("{} audio ({fam} keep)", l.name);
            l.comment = format!("{tag}{AUDIO_KEEP}{}{}", shot.0, if *mute { MUTED } else { "" });
            (l.in_point, l.out_point) = (*a, *b);
            l.switches.video = false;
            l.switches.audio = true;
            l.track_matte = None;
            if let Some(fx) = l.props.sub_mut("effects") {
                fx.children.retain(|n| !matches!(n, Node::Group(g) if prefix_of(&g.name).is_some()));
            }
            made.push((l.id, l.name.clone(), *shot, *a, *b, *mute));
            comp.layers.insert(i.saturating_add(1), l);
            if *mute && let Some(sl) = comp.layer_mut(*shot) {
                sl.switches.audio = false;
            }
        }
        proj.next_id = next;
        Ok(made)
    })?;
    let f = |t: Tick| fr.frame_at(fr.snap_nearest(t));
    Ok(made
        .into_iter()
        .map(|(id, name, shot, a, b, mute)| {
            json!({"layer": id.0, "name": name, "what": "audio-keep", "of": shot.0, "from": a.seconds(), "to": b.seconds(), "fromFrame": f(a), "toFrame": f(b), "shotAudio": if mute { "off" } else { "on" }})
        })
        .collect())
}

/// Removal: the pair's audio keep copies (`is_pair` on their comment) give their shots back the
/// span they kept (in = the earliest, out = the latest) and, when a copy turned it off, the shot's
/// own audio; each copy names its own shot.
pub(super) fn restore_kept(c: &mut Comp, is_pair: &dyn Fn(&str) -> bool) {
    let kept: Vec<(LayerId, Tick, Tick, bool)> = c
        .layers
        .iter()
        .filter(|l| is_pair(&l.comment))
        .filter_map(|l| keep_of(&l.comment).map(|shot| (shot, l.in_point, l.out_point, l.comment.ends_with(MUTED))))
        .collect();
    for (shot, a, b, muted) in kept {
        if let Some(l) = c.layer_mut(shot) {
            l.in_point = l.in_point.min(a);
            l.out_point = l.out_point.max(b);
            if muted {
                l.switches.audio = true;
            }
        }
    }
}
