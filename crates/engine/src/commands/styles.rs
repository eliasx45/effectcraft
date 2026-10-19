//! Layer ▸ Layer Styles: add / remove / show styles and the comp's Global Light.
//!
//! Styles are property groups under the layer's `layerStyles` group (see
//! [`effectcraft_project::styles`]); their values are edited with `prop.set` like any other
//! property (`layerStyles/dropShadow/distance`). Global Light is shared by every layer of a comp:
//! editing one layer's `layerStyles/blendingOptions/globalLightAngle` updates them all.
//!
//! Style families (batch 1, CX3): layers linked to a family (the 17 captions in "Caption glow +
//! double shadow") change together: `style.family.set {family, size}` scales every linked layer
//! of the project, in one undo step; `props` sets the same values on all of them.
//! `style.family.link`, `style.family.unlink`, `style.family.list`.

use effectcraft_project::build::Ids;
use effectcraft_project::styles::{self as st, GROUP, STYLES};
use effectcraft_project::{ItemId, LayerId, Uid};
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, f_p, has_comp, has_layers, layer_mut, layer_p, layers_p, str_p, time_p};
use crate::{EngineError, Result, Session, cmd};

/// Selected (or given) layers that take layer styles.
fn style_layers(s: &Session, p: &Value) -> Result<(ItemId, Vec<LayerId>)> {
    let (cid, ids) = layers_p(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    Ok((cid, ids.into_iter().filter(|id| comp.layer(*id).is_some_and(|l| l.can_have_styles())).collect()))
}

fn add_styles(s: &mut Session, p: &Value, cmd: &str, styles: &[&str], enabled: bool, label: &str) -> Result<Value> {
    let (cid, ids) = style_layers(s, p)?;
    if ids.is_empty() {
        return Err(bad(cmd, "no layer that takes layer styles (footage, solid, precomp, text or shape layer)"));
    }
    let uids = s.edit(label, None, |proj, state| {
        let gl = proj.comp(cid).ok_or(EngineError::NoComp)?.global_light.clone();
        let mut out = vec![];
        let mut sel = vec![];
        for lid in &ids {
            let mut next = proj.next_id;
            let l = layer_mut(proj, cid, *lid)?;
            for id in styles {
                let had = l.layer_styles().and_then(|g| g.sub(id)).is_some();
                let uid = st::add_style(l, &mut Ids(&mut next), id, &gl, enabled).ok_or_else(|| bad(cmd, format!("unknown style `{id}`")))?;
                // Choosing a style that is already there (e.g. after Show All) switches it on.
                if had
                    && enabled
                    && let Some(g) = l.props.find_group_mut(uid)
                {
                    g.enabled = true;
                }
                if let Some(g) = l.props.sub_mut(GROUP) {
                    g.enabled = true;
                }
                out.push(json!({"layer": lid.0, "style": id, "uid": uid}));
                sel.push((*lid, uid));
            }
            proj.next_id = next;
        }
        state.selected_props = sel;
        Ok(out)
    })?;
    Ok(json!({"styles": uids}))
}

fn add(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "style").ok_or_else(|| bad("layer.style.add", "missing `style` (e.g. dropShadow or \"Drop Shadow\")"))?;
    let id = st::style_id(name)
        .ok_or_else(|| bad("layer.style.add", format!("unknown style `{name}`; one of {}", STYLES.iter().map(|s| s.0).collect::<Vec<_>>().join(", "))))?;
    let label = STYLES.iter().find(|s| s.0 == id).map(|s| s.1).unwrap_or(id);
    add_styles(s, p, "layer.style.add", &[id], true, label)
}

macro_rules! style_cmd {
    ($fn:ident, $id:literal) => {
        fn $fn(s: &mut Session, p: &Value) -> Result<Value> {
            let label = STYLES.iter().find(|x| x.0 == $id).map(|x| x.1).unwrap_or($id);
            add_styles(s, p, concat!("layer.style.", $id), &[$id], true, label)
        }
    };
}
style_cmd!(drop_shadow, "dropShadow");
style_cmd!(inner_shadow, "innerShadow");
style_cmd!(outer_glow, "outerGlow");
style_cmd!(inner_glow, "innerGlow");
style_cmd!(bevel_emboss, "bevelEmboss");
style_cmd!(satin, "satin");
style_cmd!(color_overlay, "colorOverlay");
style_cmd!(gradient_overlay, "gradientOverlay");
style_cmd!(stroke, "stroke");

/// Show All: every style is added (new ones switched off) so they can be revealed and edited.
fn show_all(s: &mut Session, p: &Value) -> Result<Value> {
    let all: Vec<&str> = STYLES.iter().map(|s| s.0).collect();
    add_styles(s, p, "layer.style.showAll", &all, false, "Show All Layer Styles")
}

fn remove_all(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, ids) = style_layers(s, p)?;
    let n = s.edit("Remove All Layer Styles", None, |proj, state| {
        let mut n = 0;
        for lid in &ids {
            let l = layer_mut(proj, cid, *lid)?;
            if let Some(g) = l.layer_styles() {
                let gone: Vec<Uid> = std::iter::once(g.uid).chain(g.groups().map(|x| x.uid)).collect();
                l.props.children.retain(|c| c.match_id() != GROUP);
                state.selected_props.retain(|(_, u)| !gone.contains(u));
                n += 1;
            }
        }
        Ok(n)
    })?;
    Ok(json!({"layers": n}))
}

/// A style group of a layer by match id, display name, uid or the selection.
fn find_style(s: &Session, p: &Value, cmd: &str) -> Result<(ItemId, LayerId, Uid)> {
    let (cid, lid) = layer_p(s, p, cmd)?;
    let layer = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or(EngineError::NoComp)?;
    let g = layer.layer_styles().ok_or_else(|| bad(cmd, "the layer has no layer styles"))?;
    let found = match p.get("style") {
        Some(Value::Number(n)) => {
            let n = n.as_u64().unwrap_or(0);
            if n == g.uid { Some(g.uid) } else { g.groups().find(|x| x.uid == n).map(|x| x.uid) }
        }
        Some(Value::String(name)) if name == GROUP || name.eq_ignore_ascii_case("Layer Styles") => Some(g.uid),
        Some(Value::String(name)) => {
            let id = st::style_id(name).unwrap_or(name.as_str());
            g.groups().find(|x| x.match_id == id || x.name.eq_ignore_ascii_case(name)).map(|x| x.uid)
        }
        _ => s.state.selected_props.iter().find_map(|(l, u)| (*l == lid).then(|| g.groups().find(|x| x.uid == *u).map(|x| x.uid)).flatten()),
    }
    .ok_or_else(|| bad(cmd, "no such layer style"))?;
    Ok((cid, lid, found))
}

fn remove(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = find_style(s, p, "layer.style.remove")?;
    s.edit("Remove Layer Style", None, |proj, state| {
        let l = layer_mut(proj, cid, lid)?;
        let whole = l.layer_styles().is_some_and(|g| g.uid == uid);
        if whole {
            l.props.children.retain(|c| c.match_id() != GROUP);
        } else if let Some(g) = l.props.sub_mut(GROUP) {
            if g.sub(st::BLENDING).is_some_and(|b| b.uid == uid) {
                return Err(bad("layer.style.remove", "Blending Options can't be removed"));
            }
            g.children.retain(|c| c.uid() != uid);
            // The last style gone removes the Layer Styles group, as in AE.
            if g.children.iter().all(|c| c.match_id() == st::BLENDING) {
                l.props.children.retain(|c| c.match_id() != GROUP);
            }
        }
        state.selected_props.retain(|(_, u)| *u != uid);
        Ok(())
    })?;
    Ok(Value::Null)
}

/// Eye switch of a style (or of the whole Layer Styles group: `style: "layerStyles"`).
fn toggle(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid, uid) = find_style(s, p, "layer.style.toggle")?;
    let v = b_p(p, "value");
    let r = s.edit("Toggle Layer Style", None, |proj, _| {
        let g = layer_mut(proj, cid, lid)?.props.find_group_mut(uid).ok_or_else(|| bad("layer.style.toggle", "gone"))?;
        g.enabled = v.unwrap_or(!g.enabled);
        Ok(g.enabled)
    })?;
    Ok(json!(r))
}

fn global_light(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = super::comp_id(s, p)?;
    let (angle, altitude) = (f_p(p, "angle"), f_p(p, "altitude"));
    if angle.is_none() && altitude.is_none() {
        let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
        return Ok(json!({"angle": c.global_light.angle.value.as_f64(), "altitude": c.global_light.altitude.value.as_f64()}));
    }
    let t = time_p(s, p, s.project.comp(cid));
    s.edit("Global Light", super::merge_p(p), |proj, _| {
        let c = proj.comp_mut(cid).ok_or(EngineError::NoComp)?;
        st::set_global_light(c, angle, altitude, t);
        Ok(())
    })?;
    let c = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    Ok(json!({"angle": c.global_light.angle.value_at(t).as_f64(), "altitude": c.global_light.altitude.value_at(t).as_f64()}))
}

fn no_psd_styles(s: &Session) -> std::result::Result<(), String> {
    has_layers(s)?;
    Err("no Photoshop layer styles to convert (only layers imported from PSD files carry them)".into())
}

fn convert(_: &mut Session, _: &Value) -> Result<Value> {
    Err(EngineError::Other("no Photoshop layer styles to convert".into()))
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, lid) = layer_p(s, p, "layer.style.list")?;
    let l = s.project.comp(cid).and_then(|c| c.layer(lid)).ok_or(EngineError::NoComp)?;
    let styles: Vec<Value> = l
        .layer_styles()
        .map(|g| {
            g.groups()
                .filter(|x| x.match_id != st::BLENDING)
                .map(|x| json!({"style": x.match_id, "name": x.name, "uid": x.uid, "enabled": x.enabled}))
                .collect()
        })
        .unwrap_or_default();
    Ok(
        json!({"group": l.layer_styles().map(|g| g.uid), "enabled": l.layer_styles().map(|g| g.enabled), "styles": styles, "available": STYLES.iter().map(|s| json!({"style": s.0, "name": s.1})).collect::<Vec<_>>()}),
    )
}

// ── Style families (CX3) ───────────────────────────────────────────────────────────────────

// TODO(batch1-verify): the family link lives in the layer's comment as `[family: NAME @SIZE%]`
// (Layer has no metadata field yet); move it to a real field on `effectcraft_project::Layer`.
const TAG_OPEN: &str = "[family: ";

/// The family a layer's comment links it to, and the size (percent) it is at.
pub fn family_of(comment: &str) -> Option<(String, f64)> {
    let start = comment.find(TAG_OPEN)?;
    let rest = comment.get(start + TAG_OPEN.len()..)?;
    let body = rest.get(..rest.find(']')?)?;
    let (name, size) = match body.rsplit_once(" @") {
        Some((n, sz)) => (n, sz.trim_end_matches('%').trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0).unwrap_or(100.0)),
        None => (body, 100.0),
    };
    let name = name.trim();
    (!name.is_empty()).then(|| (name.to_string(), size))
}

/// `comment` without its family tag.
fn untagged(comment: &str) -> String {
    let Some(start) = comment.find(TAG_OPEN) else { return comment.to_string() };
    let end = comment.get(start..).and_then(|r| r.find(']')).map(|e| start + e + 1).unwrap_or(comment.len());
    let mut out = String::new();
    out.push_str(comment.get(..start).unwrap_or(""));
    out.push_str(comment.get(end..).unwrap_or(""));
    out.trim().to_string()
}

/// `comment` linked to `family` at `size` percent.
fn tagged(comment: &str, family: &str, size: f64) -> String {
    let base = untagged(comment);
    let tag = format!("{TAG_OPEN}{family} @{}%]", (size * 100.0).round() / 100.0);
    if base.is_empty() { tag } else { format!("{base} {tag}") }
}

fn family_p<'a>(p: &'a Value, cmd: &str) -> Result<&'a str> {
    let f = str_p(p, "family").map(str::trim).filter(|f| !f.is_empty()).ok_or_else(|| bad(cmd, "missing `family` (e.g. \"Captions\")"))?;
    if f.contains(']') || f.contains(" @") || f.chars().count() > 80 {
        return Err(bad(cmd, "`family`: a name without `]` or ` @`, at most 80 characters"));
    }
    Ok(f)
}

/// Every layer of the project linked to `family`: (comp, layer, size).
fn family_members(s: &Session, family: &str) -> Vec<(ItemId, LayerId, f64)> {
    let mut out = vec![];
    for (cid, c) in s.project.comps() {
        for l in c.layers.iter() {
            if let Some((f, size)) = family_of(&l.comment)
                && f.eq_ignore_ascii_case(family)
            {
                out.push((*cid, l.id, size));
            }
        }
    }
    out
}

fn family_link(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "style.family.link";
    let family = family_p(p, cmd)?.to_string();
    let (cid, ids) = layers_p(s, p)?;
    if ids.is_empty() {
        return Err(bad(cmd, "select the layers to link (or pass `layers`)"));
    }
    // New members join at the family's size (so the next change scales them the same way).
    let size = super::f_p(p, "size").or_else(|| family_members(s, &family).first().map(|m| m.2)).unwrap_or(100.0);
    if !(size.is_finite() && size > 0.0) {
        return Err(bad(cmd, "`size`: a percentage above 0"));
    }
    let n = s.edit("Link to Style Family", None, |proj, _| {
        for lid in &ids {
            let l = layer_mut(proj, cid, *lid)?;
            l.comment = tagged(&l.comment, &family, size);
        }
        Ok(ids.len())
    })?;
    Ok(json!({"family": family, "linked": n, "members": family_members(s, &family).len(), "size": size}))
}

fn family_unlink(s: &mut Session, p: &Value) -> Result<Value> {
    let (cid, ids) = layers_p(s, p)?;
    let n = s.edit("Unlink from Style Family", None, |proj, _| {
        let mut n = 0;
        for lid in &ids {
            let l = layer_mut(proj, cid, *lid)?;
            if family_of(&l.comment).is_some() {
                l.comment = untagged(&l.comment);
                n += 1;
            }
        }
        Ok(n)
    })?;
    Ok(json!({"unlinked": n}))
}

/// Most `props` one call sets.
const MAX_FAMILY_PROPS: usize = 32;

fn family_set(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "style.family.set";
    let family = family_p(p, cmd)?.to_string();
    let size = super::f_p(p, "size");
    if size.is_some_and(|v| !(v.is_finite() && v > 0.0 && v <= 10_000.0)) {
        return Err(bad(cmd, "`size`: a percentage above 0"));
    }
    let props: Vec<(String, Value)> = match p.get("props") {
        None | Some(Value::Null) => vec![],
        Some(Value::Object(o)) => o.iter().take(MAX_FAMILY_PROPS).map(|(k, v)| (k.clone(), v.clone())).collect(),
        Some(_) => return Err(bad(cmd, "`props`: {path: value}, e.g. {\"effects/Deep Glow/radius\": 20}")),
    };
    if size.is_none() && props.is_empty() {
        return Err(bad(cmd, "nothing to change: pass `size` and/or `props`"));
    }
    let members = family_members(s, &family);
    if members.is_empty() {
        return Err(bad(cmd, format!("no layer is linked to “{family}” (style.family.link)")));
    }
    let label = match size {
        Some(v) => format!("Style Family: {family} size {v} %"),
        None => format!("Style Family: {family}"),
    };
    let (fam, props2) = (family.clone(), props.clone());
    let skipped = s.edit(&label, None, |proj, _| {
        let mut skipped: Vec<Value> = vec![];
        for (cid, lid, old) in &members {
            let l = layer_mut(proj, *cid, *lid)?;
            if let Some(new) = size {
                super::cards::scale_layer(l, new / old);
                l.comment = tagged(&l.comment, &fam, new);
            }
            for (path, v) in &props2 {
                match l.props.prop_mut(path) {
                    Some(pr) if pr.is_animated() => skipped.push(json!({"layer": lid.0, "path": path, "why": "animated"})),
                    Some(pr) => {
                        // The value read as prop.set reads it (units, dimensions, colours).
                        let cur = pr.value.clone();
                        match super::prop::coerce_for(pr, &cur, v, cmd) {
                            Ok((kv, _)) => pr.value = kv,
                            Err(e) => skipped.push(json!({"layer": lid.0, "path": path, "why": e.to_string()})),
                        }
                    }
                    None => skipped.push(json!({"layer": lid.0, "path": path, "why": "no such property"})),
                }
            }
        }
        Ok(skipped)
    })?;
    Ok(json!({"family": family, "layers": members.len(), "size": size, "skipped": skipped}))
}

fn family_list(s: &mut Session, _p: &Value) -> Result<Value> {
    let mut fams: std::collections::BTreeMap<String, (usize, f64, Vec<Value>)> = Default::default();
    for (cid, c) in s.project.comps() {
        for l in c.layers.iter() {
            if let Some((f, size)) = family_of(&l.comment) {
                let e = fams.entry(f).or_insert((0, size, vec![]));
                e.0 += 1;
                if e.2.len() < 50 {
                    e.2.push(json!({"comp": cid.0, "layer": l.id.0, "name": l.name}));
                }
            }
        }
    }
    let list: Vec<Value> = fams.into_iter().map(|(f, (n, size, layers))| json!({"family": f, "count": n, "size": size, "layers": layers})).collect();
    Ok(json!({"families": list}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "style.family.set",
            "Change Style Family",
            [],
            None,
            "{family, size? (percent: every linked layer's Scale, keys too, follows), props? ({path: value}: set on every linked layer; animated ones are skipped)} → {family, layers, size, skipped}: every layer of the project linked to the family changes, one undo step",
            has_comp,
            family_set
        ),
        cmd!(
            "style.family.link",
            "Link to Style Family",
            [],
            None,
            "{family, layers?, size? (percent the layers are at now; default the family's)} → {family, linked, members, size}",
            has_layers,
            family_link
        ),
        cmd!("style.family.unlink", "Unlink from Style Family", [], None, "{layers?} → {unlinked}", has_layers, family_unlink),
        crate::query!("style.family.list", "Style Families", "{} → {families: [{family, count, size, layers: [{comp, layer, name}]}]}", family_list),
        cmd!("layer.style.convertToEditable", "Convert to Editable Styles", ["Layer", "Layer Styles"], None, "{layers?}", no_psd_styles, convert),
        cmd!("layer.style.showAll", "Show All", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, show_all),
        cmd!("layer.style.removeAll", "Remove All", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, remove_all),
        cmd!("layer.style.dropShadow", "Drop Shadow", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, drop_shadow),
        cmd!("layer.style.innerShadow", "Inner Shadow", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, inner_shadow),
        cmd!("layer.style.outerGlow", "Outer Glow", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, outer_glow),
        cmd!("layer.style.innerGlow", "Inner Glow", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, inner_glow),
        cmd!("layer.style.bevelEmboss", "Bevel and Emboss", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, bevel_emboss),
        cmd!("layer.style.satin", "Satin", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, satin),
        cmd!("layer.style.colorOverlay", "Color Overlay", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, color_overlay),
        cmd!("layer.style.gradientOverlay", "Gradient Overlay", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, gradient_overlay),
        cmd!("layer.style.stroke", "Stroke", ["Layer", "Layer Styles"], None, "{layers?}", has_layers, stroke),
        cmd!(
            "layer.style.add",
            "Add Layer Style",
            [],
            None,
            "{style: dropShadow|innerShadow|outerGlow|innerGlow|bevelEmboss|satin|colorOverlay|gradientOverlay|stroke (or display name), layers?}",
            has_layers,
            add
        ),
        cmd!("layer.style.remove", "Remove Layer Style", [], None, "{layer?, style: id|name|uid|layerStyles}", has_layers, remove),
        cmd!("layer.style.toggle", "Toggle Layer Style", [], None, "{layer?, style: id|name|uid|layerStyles, value?}", has_layers, toggle),
        cmd!(
            "layer.style.globalLight",
            "Global Light",
            [],
            None,
            "{comp?, angle? (deg), altitude? (deg, 0..90), time? (s)} — no values returns the current setting",
            has_comp,
            global_light
        ),
        crate::query!("layer.style.list", "List Layer Styles", "{layer?}", list),
    ]
}
