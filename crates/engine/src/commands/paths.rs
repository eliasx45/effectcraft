//! Layer ▸ Mask and Shape Path (RotoBezier, Convert To Bezier Path, Group / Ungroup Shapes, Set
//! First Vertex, Free Transform Points) and Layer ▸ Mask ▸ Motion Blur, Feather Falloff and Hide
//! Locked Masks.

use effectcraft_keyframe::{ShapePath, Value as KV};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{FeatherFalloff, GroupKind, ItemId, LayerId, LayerSource, MaskMotionBlur, Node, PropGroup, Uid};
use serde_json::{Value, json};

use super::layer_menu::target_masks;
use super::{CommandSpec, b_p, bad, f_p, has_comp, has_layers, layer_mut, layer_p, merge_p, str_p};
use crate::{EngineError, Result, Session, VertexRef, cmd};

// ---------------------------------------------------------------- RotoBezier

/// Automatic (RotoBezier) tangents: each vertex's tangents follow the line through its
/// neighbours, a third of the way to them (Catmull-Rom); open ends get none.
pub(crate) fn roto_smooth(sp: &mut ShapePath) {
    let n = sp.vertices.len();
    sp.in_tangents.resize(n, [0.0; 2]);
    sp.out_tangents.resize(n, [0.0; 2]);
    for i in 0..n {
        let (prev, next) = if sp.closed {
            ((i + n - 1) % n, (i + 1) % n)
        } else if i == 0 || i + 1 == n {
            sp.in_tangents[i] = [0.0; 2];
            sp.out_tangents[i] = [0.0; 2];
            continue;
        } else {
            (i - 1, i + 1)
        };
        let (a, b) = (sp.vertices[prev], sp.vertices[next]);
        let d = [(b[0] - a[0]) / 6.0, (b[1] - a[1]) / 6.0];
        sp.out_tangents[i] = d;
        sp.in_tangents[i] = [-d[0], -d[1]];
    }
}

/// Whether a mask uses RotoBezier tangents.
pub(crate) fn is_roto(g: &PropGroup) -> bool {
    matches!(g.kind, GroupKind::Mask { roto_bezier: true, .. })
}

/// Apply `f` to a mask path's static value and every key.
fn map_path(g: &mut PropGroup, f: &dyn Fn(&mut ShapePath)) {
    let Some(pr) = g.get_mut("path") else { return };
    if let KV::Path(sp) = &mut pr.value {
        f(sp);
    }
    for k in &mut pr.keys {
        if let KV::Path(sp) = &mut k.value {
            f(sp);
        }
    }
}

/// Mask targets: masks of the selected vertices, else [`target_masks`].
fn masks_for(s: &Session, p: &Value) -> Result<(ItemId, Vec<(LayerId, Uid)>)> {
    if p.get("mask").is_none() && p.get("layer").is_none() && !s.state.selected_vertices.is_empty() {
        let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
        let mut v: Vec<(LayerId, Uid)> = s.state.selected_vertices.iter().map(|v| (v.layer, v.mask)).collect();
        v.sort();
        v.dedup();
        return Ok((cid, v));
    }
    // A shape layer's Path item by uid.
    if let Some(uid) = p.get("mask").and_then(Value::as_u64) {
        let (cid, lid) = layer_p(s, p, "path")?;
        if s.project.comp(cid).and_then(|c| c.layer(lid)).and_then(|l| l.props.find_group(uid)).is_some_and(|g| g.match_id == "path") {
            return Ok((cid, vec![(lid, uid)]));
        }
    }
    target_masks(s, p)
}

fn edit_mask_groups(s: &mut Session, p: &Value, label: &str, f: impl Fn(&mut PropGroup) -> Result<()>) -> Result<Value> {
    let (cid, targets) = masks_for(s, p)?;
    if targets.is_empty() {
        return Err(bad(label, "no masks: select a layer with masks (or mask vertices)"));
    }
    let n = targets.len();
    s.edit(label, None, |proj, _| {
        for (lid, uid) in &targets {
            let l = layer_mut(proj, cid, *lid)?;
            if l.switches.locked {
                return Err(bad(label, "layer is locked"));
            }
            if let Some(g) = l.props.find_group_mut(*uid) {
                if super::mask::path_locked(g) {
                    return Err(bad(label, "mask is locked"));
                }
                f(g)?;
            }
        }
        Ok(())
    })?;
    Ok(json!({"masks": n}))
}

fn roto_bezier(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, targets) = masks_for(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let all_on = targets.iter().all(|(l, u)| comp.layer(*l).and_then(|l| l.masks()).and_then(|m| m.find_group(*u)).is_some_and(is_roto));
    let on = b_p(p, "value").unwrap_or(!all_on);
    edit_mask_groups(s, p, "RotoBezier", |g| {
        if let GroupKind::Mask { roto_bezier, .. } = &mut g.kind {
            *roto_bezier = on;
        }
        if on {
            map_path(g, &roto_smooth);
        }
        Ok(())
    })?;
    Ok(json!({"rotoBezier": on}))
}

fn convert_to_bezier(s: &mut Session, p: &Value) -> Result<Value> {
    let c = "path.convertToBezier";
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let primitive = |g: &PropGroup| matches!(g.match_id.as_str(), "rect" | "ellipse" | "star");
    let mut targets = Vec::new();
    if let Some(uid) = p.get("prop").and_then(Value::as_u64) {
        let (_, lid) = layer_p(s, p, c)?;
        let group = comp.layer(lid).and_then(|l| l.props.find_group(uid)).ok_or_else(|| bad(c, "no such shape item"))?;
        if !primitive(group) {
            return Err(bad(c, "select a Rectangle, Ellipse or Polystar"));
        }
        targets.push((lid, uid));
    } else if p.get("mask").is_none() {
        targets.extend(s.state.selected_props.iter().filter_map(|(lid, uid)| {
            let props = &comp.layer(*lid)?.props;
            let g = props.find_group(*uid).filter(|g| primitive(g)).or_else(|| props.parent_of(*uid).filter(|g| primitive(g)))?;
            Some((*lid, g.uid))
        }));
        if targets.is_empty() && s.state.selected_vertices.is_empty() {
            let (_, lids) = super::layers_p(s, p)?;
            for lid in lids {
                let Some(contents) = comp.layer(lid).and_then(|l| l.props.sub("contents")) else { continue };
                let mut pending = vec![(contents, 0usize)];
                while let Some((g, depth)) = pending.pop() {
                    if depth > 256 || targets.len() > 4096 {
                        return Err(bad(c, "shape conversion limit exceeded"));
                    }
                    if primitive(g) {
                        targets.push((lid, g.uid));
                    }
                    pending.extend(g.groups().map(|g| (g, depth + 1)));
                }
            }
        }
    }
    if targets.is_empty() {
        return edit_mask_groups(s, p, "Convert To Bezier Path", |g| {
            if let GroupKind::Mask { roto_bezier, .. } = &mut g.kind {
                *roto_bezier = false;
            }
            Ok(())
        });
    }
    targets.sort_unstable();
    targets.dedup();
    if targets.len() > 4096 {
        return Err(bad(c, "shape conversion limit exceeded"));
    }
    let ctx = effectcraft_render::EvalCtx {
        footage: Some(s.footage.as_ref()),
        expr: s.expr.as_deref(),
        ..effectcraft_render::EvalCtx::new(&s.project, cid, comp, s.time())
    };
    let mut converted = Vec::new();
    let snapshot = b_p(p, "snapshot").unwrap_or(false);
    for (lid, uid) in targets {
        let layer = comp.layer(lid).ok_or(EngineError::NoComp)?;
        if layer.switches.locked {
            return Err(bad(c, "layer is locked"));
        }
        let g = layer.props.find_group(uid).ok_or_else(|| bad(c, "shape item vanished"))?;
        if !snapshot && g.children.iter().filter_map(|n| n.as_prop()).any(|pr| pr.match_id != "direction" && (!pr.keys.is_empty() || pr.expr.is_some())) {
            return Err(bad(
                c,
                "animated or expression-driven geometry: use snapshot:true to explicitly convert only the current frame (undo restores the original)",
            ));
        }
        let path = effectcraft_render::shapes::primitive_path(&ctx, layer, g).ok_or_else(|| bad(c, "not a parametric path"))?;
        let mut paths = effectcraft_path::from_kurbo(&path);
        if paths.len() != 1 {
            return Err(bad(c, "primitive did not produce one editable contour"));
        }
        let path = paths.pop().ok_or_else(|| bad(c, "empty primitive"))?;
        if !path.vertices.iter().chain(&path.in_tangents).chain(&path.out_tangents).flatten().all(|v| v.is_finite()) {
            return Err(bad(c, "invalid primitive geometry"));
        }
        converted.push((lid, uid, path));
    }
    s.edit("Convert To Bezier Path", None, |proj, st| {
        let mut next = proj.next_id;
        for (lid, uid, path) in &converted {
            let layer = layer_mut(proj, cid, *lid)?;
            let g = layer.props.find_group_mut(*uid).ok_or_else(|| bad(c, "shape item vanished"))?;
            let mut replacement = build::shape_path(&mut Ids(&mut next), path.clone());
            replacement.uid = g.uid;
            replacement.name = g.name.clone();
            replacement.enabled = g.enabled;
            replacement.label = g.label;
            if let (Some(old), Some(new)) = (g.get("direction"), replacement.get_mut("direction")) {
                *new = old.clone();
            }
            *g = replacement;
        }
        proj.next_id = next;
        st.selected_props = converted.iter().map(|(l, u, _)| (*l, *u)).collect();
        st.selected_vertices.clear();
        Ok(json!({"shapes": converted.len(), "snapshot": snapshot}))
    })
}

// ---------------------------------------------------------------- mask options

fn mask_motion_blur(s: &mut Session, p: &Value) -> Result<Value> {
    let mode = match str_p(p, "mode").unwrap_or("") {
        "sameAsLayer" | "same" => MaskMotionBlur::SameAsLayer,
        "on" => MaskMotionBlur::On,
        "off" => MaskMotionBlur::Off,
        _ => return Err(bad("layer.mask.motionBlur", "mode: sameAsLayer|on|off")),
    };
    edit_mask_groups(s, p, "Mask Motion Blur", |g| {
        if let GroupKind::Mask { motion_blur, .. } = &mut g.kind {
            *motion_blur = mode;
        }
        Ok(())
    })
}

fn feather_falloff(s: &mut Session, p: &Value) -> Result<Value> {
    let mode = match str_p(p, "mode").unwrap_or("") {
        "smooth" => FeatherFalloff::Smooth,
        "linear" => FeatherFalloff::Linear,
        _ => return Err(bad("layer.mask.featherFalloff", "mode: smooth|linear")),
    };
    edit_mask_groups(s, p, "Mask Feather Falloff", |g| {
        if let GroupKind::Mask { feather_falloff, .. } = &mut g.kind {
            *feather_falloff = mode;
        }
        Ok(())
    })
}

fn hide_locked(s: &mut Session, p: &Value) -> Result<Value> {
    s.state.hide_locked_masks = b_p(p, "value").unwrap_or(!s.state.hide_locked_masks);
    Ok(json!(s.state.hide_locked_masks))
}

// ---------------------------------------------------------------- vertices

fn has_one_vertex(s: &Session) -> std::result::Result<(), String> {
    has_comp(s)?;
    if s.state.selected_vertices.len() == 1 { Ok(()) } else { Err("select one mask vertex".into()) }
}

fn set_first_vertex(s: &mut Session, p: &Value) -> Result<Value> {
    let v = match (p.get("layer"), p.get("mask"), p.get("index").and_then(Value::as_u64)) {
        (Some(_), Some(m), Some(i)) => {
            let (_, lid) = layer_p(s, p, "path.setFirstVertex")?;
            let mask = m.as_u64().ok_or_else(|| bad("path.setFirstVertex", "`mask` is the mask uid"))?;
            VertexRef { layer: lid, mask, index: i as usize }
        }
        _ => *s
            .state
            .selected_vertices
            .first()
            .filter(|_| s.state.selected_vertices.len() == 1)
            .ok_or_else(|| bad("path.setFirstVertex", "select one mask vertex"))?,
    };
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    s.edit("Set First Vertex", None, |proj, st| {
        let l = layer_mut(proj, cid, v.layer)?;
        if l.switches.locked {
            return Err(bad("path.setFirstVertex", "layer is locked"));
        }
        let g = l.props.sub_mut("masks").and_then(|m| m.find_group_mut(v.mask)).ok_or_else(|| bad("path.setFirstVertex", "no such mask"))?;
        if super::mask::path_locked(g) {
            return Err(bad("path.setFirstVertex", "mask is locked"));
        }
        let k = v.index;
        let mut err = None;
        map_path(g, &|sp| {
            if k < sp.vertices.len() && sp.closed {
                sp.vertices.rotate_left(k);
                let n = sp.in_tangents.len();
                sp.in_tangents.rotate_left(k.min(n));
                let n = sp.out_tangents.len();
                sp.out_tangents.rotate_left(k.min(n));
            }
        });
        if let Some(KV::Path(sp)) = g.get("path").map(|p| &p.value)
            && !sp.closed
        {
            err = Some(bad("path.setFirstVertex", "open paths always start at their first vertex"));
        }
        if let Some(e) = err {
            return Err(e);
        }
        st.selected_vertices = vec![VertexRef { index: 0, ..v }];
        Ok(())
    })?;
    Ok(json!({"first": v.index}))
}

/// Free Transform Points: scale / rotate / move the selected vertices (or whole masks, when fewer
/// than two of a path's points are selected) about an anchor (default: the centre of their
/// bounding box), at the current time.
fn free_transform(s: &mut Session, p: &Value) -> Result<Value> {
    let scale = match p.get("scale") {
        Some(Value::Number(n)) => {
            let k = n.as_f64().unwrap_or(100.0) / 100.0;
            [k, k]
        }
        Some(Value::Array(a)) if a.len() >= 2 => [a[0].as_f64().unwrap_or(100.0) / 100.0, a[1].as_f64().unwrap_or(100.0) / 100.0],
        _ => [1.0, 1.0],
    };
    let rot = f_p(p, "rotation").unwrap_or(0.0).to_radians();
    let off =
        p.get("offset").and_then(Value::as_array).map(|a| [a.first().and_then(Value::as_f64).unwrap_or(0.0), a.get(1).and_then(Value::as_f64).unwrap_or(0.0)]);
    let off = off.unwrap_or([0.0; 2]);
    let anchor =
        p.get("anchor").and_then(Value::as_array).map(|a| [a.first().and_then(Value::as_f64).unwrap_or(0.0), a.get(1).and_then(Value::as_f64).unwrap_or(0.0)]);
    // A general 2×3 path-space matrix [a, b, c, d, e, f] (x' = a·x + c·y + e, y' = b·x + d·y + f),
    // applied after the rest: the viewer's box scales along its own (rotated) axes with it.
    let matrix: Option<[f64; 6]> = match p.get("matrix") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) if a.len() == 6 => {
            let mut m = [0.0; 6];
            for (o, v) in m.iter_mut().zip(a) {
                *o = v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad("path.freeTransform", "matrix: six finite numbers [a, b, c, d, e, f]"))?;
            }
            Some(m)
        }
        Some(_) => return Err(bad("path.freeTransform", "matrix: six numbers [a, b, c, d, e, f]")),
    };
    if scale == [1.0, 1.0] && rot == 0.0 && off == [0.0, 0.0] && matrix.is_none() {
        return Err(bad("path.freeTransform", "pass scale (% or [x%, y%]), rotation (deg), offset [dx, dy] and/or matrix [a, b, c, d, e, f]; anchor? [x, y]"));
    }
    let merge = merge_p(p).map(str::to_string);
    let (cid, targets) = masks_for(s, p)?;
    let selected: Vec<VertexRef> = s.state.selected_vertices.clone();
    let t = s.time();
    let (c, sn) = (rot.cos(), rot.sin());
    let lin = |v: [f64; 2]| {
        let x = v[0] * scale[0];
        let y = v[1] * scale[1];
        [x * c - y * sn, x * sn + y * c]
    };
    let mat = |v: [f64; 2]| match matrix {
        Some([a, b, c, d, e, f]) => [a * v[0] + c * v[1] + e, b * v[0] + d * v[1] + f],
        None => v,
    };
    let mat_lin = |v: [f64; 2]| match matrix {
        Some([a, b, c, d, ..]) => [a * v[0] + c * v[1], b * v[0] + d * v[1]],
        None => v,
    };
    let n = s.edit("Free Transform Points", merge.as_deref(), |proj, _| {
        let mut n = 0;
        for (lid, uid) in &targets {
            let l = layer_mut(proj, cid, *lid)?;
            if l.switches.locked {
                return Err(bad("path.freeTransform", "layer is locked"));
            }
            let lt = l.layer_time(t);
            let Some(g) = l.props.find_group_mut(*uid).filter(|g| super::mask::is_path_group(g)) else { continue };
            if super::mask::path_locked(g) {
                return Err(bad("path.freeTransform", "mask is locked"));
            }
            let roto = is_roto(g);
            let Some(pr) = g.get_mut("path") else { continue };
            let KV::Path(mut sp) = pr.value_at(lt) else { continue };
            let idx: Vec<usize> = {
                let sel: Vec<usize> =
                    selected.iter().filter(|v| v.layer == *lid && v.mask == *uid).map(|v| v.index).filter(|i| *i < sp.vertices.len()).collect();
                // One point has nothing to scale or turn about: the whole path, as with none.
                if sel.len() < 2 { (0..sp.vertices.len()).collect() } else { sel }
            };
            if idx.is_empty() {
                continue;
            }
            let a = anchor.unwrap_or_else(|| {
                let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
                for v in idx.iter().filter_map(|&i| sp.vertices.get(i)) {
                    lo = [lo[0].min(v[0]), lo[1].min(v[1])];
                    hi = [hi[0].max(v[0]), hi[1].max(v[1])];
                }
                [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0]
            });
            for &i in &idx {
                let Some(v) = sp.vertices.get_mut(i) else { continue };
                let q = lin([v[0] - a[0], v[1] - a[1]]);
                *v = mat([q[0] + a[0] + off[0], q[1] + a[1] + off[1]]);
                if let Some(tn) = sp.in_tangents.get_mut(i) {
                    *tn = mat_lin(lin(*tn));
                }
                if let Some(tn) = sp.out_tangents.get_mut(i) {
                    *tn = mat_lin(lin(*tn));
                }
                n += 1;
            }
            if roto {
                roto_smooth(&mut sp);
            }
            pr.set_value_at(lt, KV::Path(sp));
        }
        Ok(n)
    })?;
    Ok(json!({"vertices": n}))
}

// ---------------------------------------------------------------- shape groups

const SHAPE_ITEMS: &[&str] = &[
    "group", "rect", "ellipse", "star", "path", "fill", "stroke", "gfill", "gstroke", "trim", "repeater", "round", "offset", "pucker", "twist", "zigzag",
    "wiggle", "merge", "front", "bevel", "side", "back",
];

/// Selected shape items (contents entries) of shape layers: (layer, uid).
fn selected_shape_items(s: &Session, p: &Value) -> Result<(ItemId, Vec<(LayerId, Uid)>)> {
    let cid = s.active_comp_id().ok_or(EngineError::NoComp)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let mut out = vec![];
    if let Some(Value::Array(a)) = p.get("items") {
        let (_, lid) = layer_p(s, p, "path.groupShapes")?;
        out.extend(a.iter().filter_map(Value::as_u64).map(|u| (lid, u)));
    } else {
        for (lid, uid) in &s.state.selected_props {
            let Some(l) = comp.layer(*lid).filter(|l| matches!(l.source, LayerSource::Shape)) else { continue };
            if l.props
                .sub("contents")
                .and_then(|c| c.find_group(*uid))
                .is_some_and(|g| g.uid != l.props.sub("contents").map(|c| c.uid).unwrap_or(0) && SHAPE_ITEMS.contains(&g.match_id.as_str()))
            {
                out.push((*lid, *uid));
            }
        }
    }
    Ok((cid, out))
}

fn has_shape_items(s: &Session) -> std::result::Result<(), String> {
    has_layers(s)?;
    let comp = s.active_comp().ok_or("no composition")?;
    let ok = s.state.selected_props.iter().any(|(l, u)| {
        comp.layer(*l).and_then(|l| l.props.sub("contents")).and_then(|c| c.find_group(*u)).is_some_and(|g| SHAPE_ITEMS.contains(&g.match_id.as_str()))
    });
    if ok { Ok(()) } else { Err("select shape items (Contents ▸ …) in the timeline".into()) }
}

fn group_shapes(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, items) = selected_shape_items(s, p)?;
    let Some(lid) = items.first().map(|x| x.0) else { return Err(bad("path.groupShapes", "select shape items to group")) };
    let uids: Vec<Uid> = items.iter().filter(|(l, _)| *l == lid).map(|(_, u)| *u).collect();
    let g = s.edit("Group Shapes", None, |proj, st| {
        let mut next = proj.next_id;
        let l = layer_mut(proj, cid, lid)?;
        let contents = l.props.sub_mut("contents").ok_or_else(|| bad("path.groupShapes", "not a shape layer"))?;
        let parent = contents.parent_of_mut(uids[0]).ok_or_else(|| bad("path.groupShapes", "no such shape item"))?;
        if !uids.iter().all(|u| parent.children.iter().any(|c| c.uid() == *u)) {
            return Err(bad("path.groupShapes", "the shape items must be in the same group"));
        }
        let at = parent.children.iter().position(|c| uids.contains(&c.uid())).unwrap_or(0);
        let mut moved = vec![];
        parent.children.retain(|c| {
            if uids.contains(&c.uid()) {
                moved.push(c.clone());
                false
            } else {
                true
            }
        });
        let n = parent.children.iter().filter(|c| c.match_id() == "group").count() + 1;
        let groups: Vec<PropGroup> = moved.into_iter().filter_map(|c| if let Node::Group(g) = c { Some(g) } else { None }).collect();
        let g = build::shape_group(&mut Ids(&mut next), &format!("Group {n}"), groups);
        let uid = g.uid;
        parent.children.insert(at.min(parent.children.len()), Node::Group(g));
        proj.next_id = next;
        st.selected_props = vec![(lid, uid)];
        Ok(uid)
    })?;
    Ok(json!({"group": g}))
}

/// Add `d` to every positional value in a shape item subtree (shape positions, path vertices,
/// group positions, gradient points, twist centres) — static values and keys.
fn offset_items(g: &mut PropGroup, d: [f64; 2]) {
    let shift = |v: &mut KV| match v {
        KV::Vec2(a) => {
            a[0] += d[0];
            a[1] += d[1];
        }
        KV::Path(sp) => {
            for q in &mut sp.vertices {
                q[0] += d[0];
                q[1] += d[1];
            }
        }
        _ => {}
    };
    let props: &[&str] = match g.match_id.as_str() {
        "rect" | "ellipse" | "star" => &["position"],
        "path" => &["path"],
        "gfill" | "gstroke" => &["start", "end"],
        "twist" => &["center"],
        _ => &[],
    };
    for m in props {
        if let Some(pr) = g.get_mut(m) {
            shift(&mut pr.value);
            for k in &mut pr.keys {
                shift(&mut k.value);
            }
        }
    }
    if g.match_id == "group"
        && let Some(pr) = g.sub_mut("transform").and_then(|t| t.get_mut("position"))
    {
        shift(&mut pr.value);
        for k in &mut pr.keys {
            shift(&mut k.value);
        }
    }
}

fn ungroup_shapes(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, items) = selected_shape_items(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let groups: Vec<(LayerId, Uid)> = items
        .into_iter()
        .filter(|(l, u)| comp.layer(*l).and_then(|l| l.props.sub("contents")).and_then(|c| c.find_group(*u)).is_some_and(|g| g.match_id == "group"))
        .collect();
    if groups.is_empty() {
        return Err(bad("path.ungroupShapes", "select a shape group to ungroup"));
    }
    let n = groups.len();
    s.edit("Ungroup Shapes", None, |proj, st| {
        let mut sel = vec![];
        for (lid, uid) in &groups {
            let l = layer_mut(proj, cid, *lid)?;
            let Some(contents) = l.props.sub_mut("contents") else { continue };
            let Some(parent) = contents.parent_of_mut(*uid) else { continue };
            let Some(i) = parent.children.iter().position(|c| c.uid() == *uid) else { continue };
            let Node::Group(g) = parent.children.remove(i) else { continue };
            // The group's transform must be a plain (static) move: it is baked into the items.
            let tr = g.sub("transform");
            let get = |m: &str| tr.and_then(|t| t.get(m));
            let animated = tr.is_some_and(|t| t.any_animated());
            let v2 = |m: &str, d: [f64; 2]| get(m).map(|p| p.value.as_vec2()).unwrap_or(d);
            let f = |m: &str, d: f64| get(m).map(|p| p.value.as_f64()).unwrap_or(d);
            let (a, pos, sc) = (v2("anchor", [0.0; 2]), v2("position", [0.0; 2]), v2("scale", [100.0; 2]));
            if animated || sc != [100.0, 100.0] || f("rotation", 0.0) != 0.0 || f("skew", 0.0) != 0.0 || f("opacity", 100.0) != 100.0 {
                return Err(bad(
                    "path.ungroupShapes",
                    format!("`{}` has a scale, rotation, skew, opacity or animated transform: reset it before ungrouping", g.name),
                ));
            }
            let d = [pos[0] - a[0], pos[1] - a[1]];
            let mut inner: Vec<Node> = g.sub("contents").map(|c| c.children.clone()).unwrap_or_default();
            for c in &mut inner {
                if let Node::Group(ig) = c {
                    if d != [0.0, 0.0] {
                        offset_items(ig, d);
                    }
                    sel.push((*lid, ig.uid));
                }
            }
            for (k, c) in inner.into_iter().enumerate() {
                parent.children.insert(i + k, c);
            }
        }
        st.selected_props = sel;
        Ok(())
    })?;
    Ok(json!({"ungrouped": n}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!("path.rotoBezier", "RotoBezier", ["Layer", "Mask and Shape Path"], None, "{layer?, mask?, value?}", has_layers, roto_bezier),
        cmd!(
            "path.convertToBezier",
            "Convert To Bezier Path",
            ["Layer", "Mask and Shape Path"],
            None,
            "{layer?, mask?, prop?: primitive uid, snapshot?: true (explicit current-frame conversion of animated geometry)}",
            has_layers,
            convert_to_bezier
        ),
        cmd!("path.groupShapes", "Group Shapes", ["Layer"], Some("Cmd+G"), "{layer?, items?: [uid]}", has_shape_items, group_shapes),
        cmd!("path.ungroupShapes", "Ungroup Shapes", ["Layer"], Some("Cmd+Shift+G"), "{layer?, items?: [group uid]}", has_shape_items, ungroup_shapes),
        cmd!(
            "path.setFirstVertex",
            "Set First Vertex",
            ["Layer", "Mask and Shape Path"],
            None,
            "{layer?, mask?: uid, index?}",
            has_one_vertex,
            set_first_vertex
        ),
        cmd!(
            "path.freeTransform",
            "Free Transform Points",
            ["Layer", "Mask and Shape Path"],
            None,
            "{layer?, mask?, scale?: % | [x%, y%], rotation?: deg, offset?: [dx, dy], anchor?: [x, y], matrix?: [a, b, c, d, e, f]}",
            has_layers,
            free_transform
        ),
        cmd!("layer.mask.motionBlur", "Motion Blur", [], None, "{layer?, mask?, mode: sameAsLayer|on|off}", has_layers, mask_motion_blur),
        cmd!("layer.mask.featherFalloff", "Feather Falloff", [], None, "{layer?, mask?, mode: smooth|linear}", has_layers, feather_falloff),
        cmd!("layer.mask.hideLocked", "Hide Locked Masks", ["Layer", "Mask"], None, "{value?}", has_comp, hide_locked),
    ]
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    fn fixture(kind: &str) -> (Session, LayerId, Uid) {
        let mut s = Session::default();
        s.execute("comp.new", json!({"width": 320,"height":180})).unwrap();
        let lid = LayerId(s.execute("layer.newShape", json!({"kind":kind})).unwrap()["layer"].as_u64().unwrap());
        let layer = s.active_comp().unwrap().layer(lid).unwrap();
        let g = layer.props.group("contents/group/contents").unwrap().groups().find(|g| matches!(g.match_id.as_str(), "rect" | "ellipse" | "star")).unwrap();
        let uid = g.uid;
        (s, lid, uid)
    }

    #[test]
    fn primitive_conversion_preserves_geometry_identity_and_undo() {
        for kind in ["rect", "ellipse", "star"] {
            let (mut s, lid, uid) = fixture(kind);
            let cid = s.active_comp_id().unwrap();
            let layer = s.active_comp().unwrap().layer(lid).unwrap();
            let old = layer.props.find_group(uid).unwrap().clone();
            let ctx = effectcraft_render::EvalCtx::new(&s.project, cid, s.active_comp().unwrap(), s.time());
            let expected = effectcraft_path::from_kurbo(&effectcraft_render::shapes::primitive_path(&ctx, layer, &old).unwrap());
            if kind == "rect" {
                s.execute("prop.select", json!({"layer":lid.0,"prop":old.get("size").unwrap().uid})).unwrap();
                s.execute("path.convertToBezier", json!({})).unwrap();
            } else {
                s.execute("path.convertToBezier", json!({"layer":lid.0,"prop":uid})).unwrap();
            }
            let now = s.active_comp().unwrap().layer(lid).unwrap().props.find_group(uid).unwrap();
            assert_eq!(now.match_id, "path");
            assert_eq!(now.name, old.name);
            assert_eq!(now.get("path").unwrap().value.as_path(), expected.first());
            assert_eq!(now.get("direction"), old.get("direction"));
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(s.active_comp().unwrap().layer(lid).unwrap().props.find_group(uid), Some(&old));
        }
    }

    #[test]
    fn animated_primitive_requires_explicit_snapshot_and_undo_restores_it() {
        let (mut s, lid, uid) = fixture("rect");
        let size = s.active_comp().unwrap().layer(lid).unwrap().props.find_group(uid).unwrap().get("size").unwrap().uid;
        s.execute("prop.setExpression", json!({"layer":lid.0,"prop":size,"expression":"[120,80]"})).unwrap();
        let before = s.active_comp().unwrap().layer(lid).unwrap().props.find_group(uid).unwrap().clone();
        assert!(s.execute("path.convertToBezier", json!({"layer":lid.0,"prop":uid})).is_err());
        assert_eq!(s.active_comp().unwrap().layer(lid).unwrap().props.find_group(uid), Some(&before));
        s.execute("path.convertToBezier", json!({"layer":lid.0,"prop":uid,"snapshot":true})).unwrap();
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.active_comp().unwrap().layer(lid).unwrap().props.find_group(uid), Some(&before));
    }
}
