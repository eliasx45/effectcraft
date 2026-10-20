//! The FigCraft link (like Dynamic Link): FigCraft shows and plays EffectCraft comps on its canvas.
//! This module is the document side: `comp.sendToFigCraft` (the range to send and the
//! `effectcraft.place` params FigCraft gets) and the precomp tree `figcraft.tree` answers. The
//! control-channel methods (`figcraft.frames`, `figcraft.audio`, `figcraft.tree`,
//! `figcraft.reveal`) and the connection to FigCraft live in the desktop app
//! (`effectcraft-ui-egui`'s `figcraft_link`). See docs/control-protocol.md, "FigCraft link".

use effectcraft_project::{Comp, ItemId, LayerId, LayerSource, Project};
use effectcraft_time::Tick;
use serde_json::{Value, json};

use super::{CommandSpec, bad, comp_id, frontend};
use crate::{EngineError, Result, Session, cmd};

const C: &str = "comp.sendToFigCraft";

/// `figcraft.tree`'s `depth`: levels of precomps below the comp (default 3).
pub const TREE_DEPTH_MAX: u32 = 4;
/// The most precomp layers one `figcraft.tree` reply lists (the rest are left out).
const TREE_MAX_NODES: usize = 2000;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "comp.sendToFigCraft",
        "Send to FigCraft",
        ["Composition"],
        None,
        "{comp?, start? (s), end? (s)} → {comp (name), compId, project (.ecproj path or empty), start, end (s), fps, width, height, name}: FigCraft's `effectcraft.place` params. The range is the selected layers' span (min In to max Out point, clipped to the comp) when the comp's layers are selected, else its work area. The desktop app sends it to FigCraft (127.0.0.1, port FIGCRAFT_CONTROL_PORT or 7979) and shows the outcome; a headless session only returns it",
        can_send,
        send
    )]
}

/// A comp is open, or one is selected in the Project panel.
fn can_send(s: &Session) -> std::result::Result<(), String> {
    if s.active_comp().is_some() || selected_comp(s).is_some() { Ok(()) } else { Err("open or select a composition first".into()) }
}

fn selected_comp(s: &Session) -> Option<ItemId> {
    s.state.project_selection.iter().copied().find(|i| s.project.comp(*i).is_some())
}

fn send(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = match p.get("comp") {
        Some(v) if v.is_number() || v.is_string() => comp_id(s, p)?,
        Some(v) if !v.is_null() => return Err(bad(C, "`comp` is a composition id or name")),
        _ => s.active_comp_id().or_else(|| selected_comp(s)).ok_or(EngineError::NoComp)?,
    };
    let seconds = |k: &str| -> Result<Option<Tick>> {
        match p.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_f64().filter(|x| x.is_finite()).map(|x| Some(Tick::from_seconds_f64(x))).ok_or_else(|| bad(C, format!("`{k}` must be seconds"))),
        }
    };
    let (start, end) = (seconds("start")?, seconds("end")?);
    let payload = {
        let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
        // The selection belongs to the active comp: another comp sends its work area.
        let selected: &[LayerId] = if s.active_comp_id() == Some(cid) { &s.state.selected_layers } else { &[] };
        let (a, b) = send_range(comp, selected);
        let dur = comp.duration.max(Tick::ZERO);
        let (a, b) = (start.unwrap_or(a).max(Tick::ZERO).min(dur), end.unwrap_or(b).max(Tick::ZERO).min(dur));
        if b <= a {
            return Err(bad(C, "the range to send is empty"));
        }
        place_params(&s.project, s.path.as_deref(), cid, a, b).ok_or(EngineError::NoComp)?
    };
    frontend(s, C, &payload)?;
    Ok(payload)
}

/// The span `comp.sendToFigCraft` sends: the selected layers' span (the earliest In point to the
/// latest Out point) clipped to the comp, else the work area, else the whole comp.
pub fn send_range(comp: &Comp, selected: &[LayerId]) -> (Tick, Tick) {
    let end = comp.duration.max(Tick::ZERO);
    let clip = |t: Tick| t.max(Tick::ZERO).min(end);
    let span = comp.layers.iter().filter(|l| selected.contains(&l.id)).fold(None, |acc: Option<(Tick, Tick)>, l| {
        let (a, b) = (l.in_point.min(l.out_point), l.in_point.max(l.out_point));
        Some(match acc {
            Some((x, y)) => (x.min(a), y.max(b)),
            None => (a, b),
        })
    });
    if let Some((a, b)) = span {
        let (a, b) = (clip(a), clip(b));
        if a < b {
            return (a, b);
        }
    }
    let (a, b) = (clip(comp.work_area.0), clip(comp.work_area.1));
    if a < b { (a, b) } else { (Tick::ZERO, end) }
}

/// `m:ss` of a time (whole seconds, rounded down).
pub fn mss(t: Tick) -> String {
    let s = t.seconds().max(0.0).floor() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// FigCraft's `effectcraft.place` params for `comp` from `start` to `end`.
pub fn place_params(project: &Project, path: Option<&str>, comp: ItemId, start: Tick, end: Tick) -> Option<Value> {
    let c = project.comp(comp)?;
    let name = project.item(comp)?.name.clone();
    let label = format!("{name} · {}–{}", mss(start), mss(end));
    Some(json!({
        "comp": name,
        "compId": comp.0,
        "project": path.unwrap_or(""),
        "start": start.seconds(),
        "end": end.seconds(),
        "fps": c.frame_rate.as_f64(),
        "width": c.width,
        "height": c.height,
        "name": label,
    }))
}

/// `{id, name, width, height, frameRate, duration}` of a comp.
pub fn comp_info(project: &Project, id: ItemId) -> Option<Value> {
    let c = project.comp(id)?;
    let name = project.item(id).map(|i| i.name.as_str()).unwrap_or_default();
    Some(json!({"id": id.0, "name": name, "width": c.width, "height": c.height, "frameRate": c.frame_rate.as_f64(), "duration": c.duration.seconds()}))
}

/// `figcraft.tree`: `comp` and, `depth` levels deep, the precomp layers that show between `start`
/// and `end` (comp time) — `{comp, children: [{layer: {id, name}, comp, start, stretch, in, out,
/// timeRemap, children}]}`. A child's times are in its parent comp; its own children are those
/// showing in the part of it its layer shows (the whole comp under time remapping).
pub fn tree(project: &Project, comp: ItemId, start: Tick, end: Tick, depth: u32) -> Option<Value> {
    let info = comp_info(project, comp)?;
    let mut budget = TREE_MAX_NODES;
    let children = tree_children(project, comp, start, end, depth.min(TREE_DEPTH_MAX), &mut budget);
    Some(json!({"comp": info, "children": children}))
}

fn tree_children(project: &Project, comp: ItemId, a: Tick, b: Tick, depth: u32, budget: &mut usize) -> Vec<Value> {
    let Some(c) = project.comp(comp) else { return vec![] };
    if depth == 0 {
        return vec![];
    }
    let mut out = vec![];
    for l in c.layers.iter() {
        let LayerSource::Comp { item } = &l.source else { continue };
        let item = *item;
        let Some(sub) = project.comp(item) else { continue };
        if l.out_point <= l.in_point || l.in_point >= b || l.out_point <= a {
            continue;
        }
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let remap = l.props.get("timeRemap").is_some();
        let (ca, cb) = if remap {
            (Tick::ZERO, sub.duration)
        } else {
            let (x, y) = (l.layer_time(l.in_point.max(a)), l.layer_time(l.out_point.min(b)));
            (x.min(y), x.max(y))
        };
        let Some(info) = comp_info(project, item) else { continue };
        let children = tree_children(project, item, ca, cb, depth - 1, budget);
        out.push(json!({
            "layer": {"id": l.id.0, "name": l.name},
            "comp": info,
            "start": l.start_time.seconds(),
            "stretch": l.stretch,
            "in": l.in_point.seconds(),
            "out": l.out_point.seconds(),
            "timeRemap": remap,
            "children": children,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use effectcraft_color::Label;
    use effectcraft_project::{Comp, ItemId, ItemKind, LayerId, LayerSource, Project};
    use effectcraft_time::{FrameRate, Tick};
    use serde_json::json;

    use super::*;
    use crate::Session;

    fn secs(s: f64) -> Tick {
        Tick::from_seconds_f64(s)
    }

    fn comp(p: &mut Project, name: &str, duration: f64) -> ItemId {
        p.add_item(name, Label::default(), None, ItemKind::Comp(Arc::new(Comp::new(320, 180, FrameRate::FPS_25, secs(duration)))))
    }

    /// A layer of `child` in `parent`: its start, In and Out (parent seconds) and stretch.
    fn nest(p: &mut Project, parent: ItemId, child: ItemId, name: &str, t: [f64; 3], stretch: f64) -> LayerId {
        let host = p.comp(parent).unwrap().clone();
        let mut l = effectcraft_project::build::layer(p, &host, name, LayerSource::Comp { item: child }, (320, 180), None);
        (l.start_time, l.in_point, l.out_point, l.stretch) = (secs(t[0]), secs(t[1]), secs(t[2]), stretch);
        let id = l.id;
        p.comp_mut(parent).unwrap().layers.push(l);
        id
    }

    #[test]
    fn tree_lists_nested_precomps_that_show_in_the_range() {
        let mut p = Project::default();
        let (main, a, b, leaf) = (comp(&mut p, "Main", 20.0), comp(&mut p, "A", 10.0), comp(&mut p, "B", 10.0), comp(&mut p, "Leaf", 10.0));
        // A shows 2–8 s of Main from its time 0; B shows 12–16 s at half speed (stretch 200).
        let la = nest(&mut p, main, a, "A layer", [2.0, 2.0, 8.0], 100.0);
        nest(&mut p, main, b, "B layer", [10.0, 12.0, 16.0], 200.0);
        // Leaf shows 0–1 s of A (A's time) and 5.5–9 s of A.
        nest(&mut p, a, leaf, "early", [0.0, 0.0, 1.0], 100.0);
        nest(&mut p, a, leaf, "late", [5.0, 5.5, 9.0], 100.0);
        // A solid in A is not a precomp: left out.
        let host = p.comp(a).unwrap().clone();
        let solid = effectcraft_project::build::layer(&mut p, &host, "Text", LayerSource::Text, (10, 10), None);
        p.comp_mut(a).unwrap().layers.push(solid);

        let all = tree(&p, main, Tick::ZERO, secs(20.0), 3).unwrap();
        assert_eq!(all["comp"], json!({"id": main.0, "name": "Main", "width": 320, "height": 180, "frameRate": 25.0, "duration": 20.0}));
        let kids = all["children"].as_array().unwrap();
        assert_eq!(kids.len(), 2, "{all}");
        assert_eq!(kids[0]["layer"], json!({"id": la.0, "name": "A layer"}));
        assert_eq!((kids[0]["start"].as_f64(), kids[0]["in"].as_f64(), kids[0]["out"].as_f64()), (Some(2.0), Some(2.0), Some(8.0)));
        assert_eq!(kids[0]["timeRemap"], false);
        assert_eq!(kids[1]["stretch"], 200.0);
        // A layer shows A's 0–6 s: both Leaf layers overlap it, times in A.
        let grand = kids[0]["children"].as_array().unwrap();
        assert_eq!(grand.iter().map(|g| g["layer"]["name"].as_str().unwrap()).collect::<Vec<_>>(), ["early", "late"]);
        assert_eq!(grand[1]["in"], 5.5);
        assert_eq!(grand[1]["comp"]["name"], "Leaf");

        // Main 2–4 s is A's 0–2 s: only "early" shows there; B doesn't show at all.
        let part = tree(&p, main, secs(2.0), secs(4.0), 3).unwrap();
        let kids = part["children"].as_array().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0]["children"].as_array().unwrap().len(), 1);
        assert_eq!(kids[0]["children"][0]["layer"]["name"], "early");

        // Depth 1: no grandchildren; depth is capped.
        let shallow = tree(&p, main, Tick::ZERO, secs(20.0), 1).unwrap();
        assert!(shallow["children"][0]["children"].as_array().unwrap().is_empty());
        assert_eq!(tree(&p, main, Tick::ZERO, secs(20.0), 99), tree(&p, main, Tick::ZERO, secs(20.0), TREE_DEPTH_MAX));
        // Not a comp.
        assert!(tree(&p, ItemId(987_654), Tick::ZERO, secs(1.0), 3).is_none());
        // A reversed range lists nothing, without panicking.
        assert!(tree(&p, main, secs(9.0), secs(1.0), 3).unwrap()["children"].as_array().unwrap().is_empty());
    }

    #[test]
    fn tree_follows_stretch_and_time_remapping_into_the_child() {
        let mut p = Project::default();
        let (main, a, leaf) = (comp(&mut p, "Main", 20.0), comp(&mut p, "A", 10.0), comp(&mut p, "Leaf", 10.0));
        // Stretch 200: Main 0–4 s is A's 0–2 s, which misses the Leaf layer at A's 3–4 s.
        let la = nest(&mut p, main, a, "A", [0.0, 0.0, 20.0], 200.0);
        nest(&mut p, a, leaf, "leaf", [3.0, 3.0, 4.0], 100.0);
        assert!(tree(&p, main, Tick::ZERO, secs(4.0), 3).unwrap()["children"][0]["children"].as_array().unwrap().is_empty());
        assert_eq!(tree(&p, main, Tick::ZERO, secs(8.0), 3).unwrap()["children"][0]["children"].as_array().unwrap().len(), 1);
        // Time remapping can show any of A: its whole comp counts.
        let mut ids = effectcraft_project::build::Ids(&mut p.next_id);
        let remap = ids.prop("timeRemap", "Time Remap", effectcraft_project::Value::Scalar(0.0));
        p.comp_mut(main).unwrap().layer_mut(la).unwrap().props.children.push(effectcraft_project::Node::Prop(remap));
        let t = tree(&p, main, Tick::ZERO, secs(4.0), 3).unwrap();
        assert_eq!(t["children"][0]["timeRemap"], true);
        assert_eq!(t["children"][0]["children"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn send_range_is_the_selected_layers_span_else_the_work_area() {
        let mut p = Project::default();
        let (main, a) = (comp(&mut p, "Main", 10.0), comp(&mut p, "A", 10.0));
        let l1 = nest(&mut p, main, a, "one", [0.0, 2.0, 5.0], 100.0);
        let l2 = nest(&mut p, main, a, "two", [0.0, 4.0, 14.0], 100.0);
        let far = nest(&mut p, main, a, "far", [20.0, 20.0, 25.0], 100.0);
        p.comp_mut(main).unwrap().work_area = (secs(1.0), secs(3.0));
        let c = p.comp(main).unwrap();
        assert_eq!(send_range(c, &[l1]), (secs(2.0), secs(5.0)));
        // Several layers: the earliest In to the latest Out, clipped to the comp.
        assert_eq!(send_range(c, &[l2, l1]), (secs(2.0), secs(10.0)));
        // Nothing selected, or only layers outside the comp: the work area.
        assert_eq!(send_range(c, &[]), (secs(1.0), secs(3.0)));
        assert_eq!(send_range(c, &[far]), (secs(1.0), secs(3.0)));
        assert_eq!(send_range(c, &[LayerId(424_242)]), (secs(1.0), secs(3.0)));
        // An empty work area: the whole comp.
        p.comp_mut(main).unwrap().work_area = (secs(4.0), secs(4.0));
        assert_eq!(send_range(p.comp(main).unwrap(), &[]), (Tick::ZERO, secs(10.0)));
        assert_eq!([mss(secs(0.0)), mss(secs(75.9)), mss(secs(-3.0)), mss(secs(f64::NAN))], ["0:00", "1:15", "0:00", "0:00"]);
    }

    #[test]
    fn send_to_figcraft_hands_the_place_params_to_the_frontend() {
        let mut s = Session::default();
        assert!(s.execute(C, json!({})).is_err(), "no comp");
        let cid =
            s.execute("comp.new", json!({"name": "Promo", "width": 640, "height": 360, "frameRate": 25, "duration": 10})).unwrap()["comp"].as_u64().unwrap();
        let l = s.execute("layer.newSolid", json!({})).unwrap()["layer"].as_u64().unwrap();
        s.execute("layer.timing", json!({"layers": [l], "in": 1.0, "out": 3.0})).unwrap();
        s.execute("layer.select", json!({"layers": [l]})).unwrap();
        s.drain_events();
        let v = s.execute(C, json!({})).unwrap();
        assert_eq!(v["comp"], "Promo");
        assert_eq!(v["compId"], cid);
        assert_eq!((v["start"].as_f64(), v["end"].as_f64(), v["fps"].as_f64()), (Some(1.0), Some(3.0), Some(25.0)));
        assert_eq!((v["width"].as_u64(), v["height"].as_u64()), (Some(640), Some(360)));
        assert_eq!(v["project"], "");
        assert_eq!(v["name"], "Promo · 0:01–0:03");
        let events = s.drain_events();
        assert!(events.iter().any(|e| matches!(e, crate::Event::Frontend { command, params } if command == C && params == &v)), "the desktop app sends it");
        // Nothing selected: the work area (the whole comp here); explicit times win, clipped.
        s.state.selected_layers.clear();
        let v = s.execute(C, json!({"comp": cid})).unwrap();
        assert_eq!((v["start"].as_f64(), v["end"].as_f64()), (Some(0.0), Some(10.0)));
        let v = s.execute(C, json!({"start": 2.5, "end": 99})).unwrap();
        assert_eq!((v["start"].as_f64(), v["end"].as_f64()), (Some(2.5), Some(10.0)));
        // Junk is refused, never a panic.
        for junk in [
            json!({"start": "x"}),
            json!({"end": [1]}),
            json!({"start": 5, "end": 5}),
            json!({"comp": "Nope"}),
            json!({"comp": {"a": 1}}),
            json!({"start": 1e308, "end": -1e308}),
        ] {
            assert!(s.execute(C, junk.clone()).is_err(), "{junk}");
        }
        assert!(s.execute_checked(C, json!({"bogus": 1})).is_err());
    }
}
