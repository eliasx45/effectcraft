//! `comp.recipe` / `comp.fromRecipe`: a composition as a compact, readable recipe of what was
//! authored, and a new composition built back from one (docs/agents.md, "Reuse a library comp").
//!
//! A `.ecproj` stores every property of every layer; a recipe keeps only what differs from what
//! EffectCraft would create anyway. Defaults are not stored on properties, so the recipe compares
//! each layer with a *skeleton*: the same kind of layer built fresh, with the same structure
//! (shape contents, effects, masks, layer styles, text animators) built by the same factories
//! the menus use. `comp.fromRecipe` builds exactly that skeleton again and applies the recorded
//! differences, so the round trip is exact up to the rounding of values (4 decimals; times are
//! kept exact).
//!
//! Anything the factories can't build (an unknown effect, a stroke with extra dashes, a tracker…)
//! is kept as a `raw` node: the property subtree itself, without uids. What can't round-trip at
//! all is listed in `unsupported` with the reason.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use effectcraft_color::{BlendMode, Label};
use effectcraft_keyframe::{Ease, Eases, Interp, Keyframe, ShapePath, TextDoc, Value as KV};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{
    Comp, Expression, Footage, GroupKind, ItemId, ItemKind, Layer, LayerId, LayerSource, MaskMode, Node, ParamUi, Project, PropGroup, Property, Renderer,
    Solid, Switches, TrackMatte, limits, styles,
};
use effectcraft_time::{FrameRate, Tick};
use serde::Serialize;
use serde_json::{Map, Value as J, json};

use super::{CommandSpec, b_p, bad, comp_id, resolve_layer, str_p};
use crate::{EngineError, Result, Session};

/// `format` of a recipe document.
pub const FORMAT: &str = "effectcraft-recipe";
/// Recipe format version.
pub const VERSION: u64 = 1;

const CMD_RECIPE: &str = "comp.recipe";
const CMD_BUILD: &str = "comp.fromRecipe";
/// Nested precomps followed at most this deep.
const MAX_PRECOMP_DEPTH: usize = 16;
/// Property trees walked at most this deep.
const MAX_TREE_DEPTH: usize = 64;
/// Limits on what a recipe may ask for.
const MAX_LAYERS: usize = 10_000;
const MAX_PRECOMPS: usize = 512;
const MAX_ITEMS: usize = 10_000;

type R<T> = std::result::Result<T, String>;

// ---------------------------------------------------------------- numbers, times, JSON

/// `x` rounded to 4 decimals (−0 is 0; huge and non-finite values unchanged).
fn r4(x: f64) -> f64 {
    if !x.is_finite() || x.abs() > 1e11 {
        return x;
    }
    let r = (x * 1e4).round() / 1e4;
    if r == 0.0 { 0.0 } else { r }
}

/// A finite number as JSON: integral values as integers (`610`, not `610.0`).
fn exact_num(x: f64) -> J {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 { J::from(x as i64) } else { J::from(x) }
}

/// `x` rounded to 4 decimals, as JSON.
fn num(x: f64) -> J {
    exact_num(r4(x))
}

fn nums(a: &[f64]) -> J {
    J::Array(a.iter().map(|x| num(*x)).collect())
}

fn to_json<T: Serialize>(v: &T) -> J {
    serde_json::to_value(v).unwrap_or(J::Null)
}

/// A copy with every non-integer number rounded to 4 decimals.
fn rounded(v: &J) -> J {
    match v {
        J::Number(n) if !(n.is_i64() || n.is_u64()) => n.as_f64().map_or(J::Null, num),
        J::Array(a) => J::Array(a.iter().map(rounded).collect()),
        J::Object(o) => J::Object(o.iter().map(|(k, v)| (k.clone(), rounded(v))).collect()),
        v => v.clone(),
    }
}

/// What of `orig` differs from `base` (objects key by key, anything else whole; `uid`s are
/// ignored), rounded; a key `base` has and `orig` lacks is `null`. `None` when they're equal.
fn diff_json(orig: &J, base: &J) -> Option<J> {
    match (orig, base) {
        (J::Object(a), J::Object(b)) => {
            let mut out = Map::new();
            for (k, va) in a {
                if k == "uid" {
                    continue;
                }
                match b.get(k) {
                    Some(vb) => {
                        if let Some(d) = diff_json(va, vb) {
                            out.insert(k.clone(), d);
                        }
                    }
                    None => {
                        out.insert(k.clone(), rounded(va));
                    }
                }
            }
            for k in b.keys() {
                if k != "uid" && !a.contains_key(k) {
                    out.insert(k.clone(), J::Null);
                }
            }
            (!out.is_empty()).then_some(J::Object(out))
        }
        _ => {
            let ra = rounded(orig);
            (ra != rounded(base)).then_some(ra)
        }
    }
}

/// Apply a [`diff_json`] patch: objects merge key by key, `null` removes the key (it reads
/// back as its default), anything else replaces.
fn merge_json(base: &mut J, patch: &J) {
    match (base, patch) {
        (J::Object(b), J::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                } else if let Some(bv) = b.get_mut(k).filter(|bv| bv.is_object() && v.is_object()) {
                    merge_json(bv, v);
                } else {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn round_to(x: f64, decimals: i32) -> f64 {
    let m = 10f64.powi(decimals);
    (x * m).round() / m
}

/// Seconds → ticks. A time written with at most 4 decimals that lies within 0.05 ms of a frame
/// of `fr` is that frame exactly (`0.2669` at 29.97 fps is frame 8, 0.266933… s).
fn time_dec(s: f64, fr: FrameRate) -> Tick {
    let t = Tick::from_seconds_f64(s);
    let short = ((s * 1e4).round() - s * 1e4).abs() < 1e-6;
    if short && fr.num > 0 && fr.den > 0 {
        let f = fr.snap_nearest(t);
        if f.0.saturating_sub(t.0).unsigned_abs() <= Tick::from_seconds_f64(0.00005).0.unsigned_abs() {
            return f;
        }
    }
    t
}

/// Ticks → the shortest seconds that [`time_dec`] reads back as exactly `t`.
fn time_enc(t: Tick, fr: FrameRate) -> J {
    let s = t.seconds();
    for d in 0..=12 {
        let v = round_to(s, d);
        if time_dec(v, fr) == t {
            return exact_num(v);
        }
    }
    exact_num(s)
}

fn time_p(j: &J, fr: FrameRate, what: &str) -> R<Tick> {
    j.as_f64().filter(|x| x.is_finite()).map(|s| time_dec(s, fr)).ok_or_else(|| format!("`{what}` must be a number of seconds"))
}

fn rate_enc(fr: FrameRate) -> J {
    let v = r4(fr.as_f64());
    if FrameRate::from_f64(v) == fr { num(v) } else { json!({"num": fr.num, "den": fr.den}) }
}

fn rate_dec(j: &J) -> R<FrameRate> {
    let fr = match j {
        J::Object(o) => FrameRate::new(o.get("num").and_then(J::as_i64).unwrap_or(0), o.get("den").and_then(J::as_i64).unwrap_or(0)),
        j => FrameRate::from_f64(j.as_f64().filter(|x| x.is_finite() && *x > 0.0).ok_or("`frameRate` must be frames per second or {num, den}")?),
    };
    if fr.num > 0 && fr.den > 0 && fr.as_f64() <= 1000.0 { Ok(fr) } else { Err("`frameRate` must be between 0 and 1000 fps".into()) }
}

// ---------------------------------------------------------------- property tree helpers

/// Same tree shape: the same match ids, node types, value kinds and group kinds, in order.
fn same_shape(a: &Node, b: &Node, depth: usize) -> bool {
    match (a, b) {
        (Node::Prop(x), Node::Prop(y)) => x.match_id == y.match_id && std::mem::discriminant(&x.value) == std::mem::discriminant(&y.value),
        (Node::Group(x), Node::Group(y)) => same_group(x, y, depth),
        _ => false,
    }
}

fn same_group(x: &PropGroup, y: &PropGroup, depth: usize) -> bool {
    let kinds = match (&x.kind, &y.kind) {
        (GroupKind::Effect { effect: a }, GroupKind::Effect { effect: b }) => a == b,
        (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
    };
    depth < MAX_TREE_DEPTH
        && kinds
        && x.match_id == y.match_id
        && x.children.len() == y.children.len()
        && x.children.iter().zip(&y.children).all(|(a, b)| same_shape(a, b, depth + 1))
}

/// A node as JSON without its uids (a `raw` recipe entry).
fn raw_json(n: &Node) -> J {
    fn strip(v: &mut J) {
        if let J::Object(o) = v {
            if o.contains_key("node") {
                o.remove("uid");
            }
            if let Some(J::Array(c)) = o.get_mut("children") {
                c.iter_mut().for_each(strip);
            }
        }
    }
    let mut j = to_json(n);
    strip(&mut j);
    j
}

/// A `raw` entry back as a node with fresh uids.
fn raw_node(j: &J, ids: &mut Ids) -> R<Node> {
    fn add(v: &mut J, depth: usize) -> R<()> {
        if depth > MAX_TREE_DEPTH {
            return Err("raw node nested too deep".into());
        }
        if let J::Object(o) = v {
            if o.contains_key("node") {
                o.insert("uid".into(), J::from(0));
            }
            if let Some(J::Array(c)) = o.get_mut("children") {
                for x in c.iter_mut() {
                    add(x, depth + 1)?;
                }
            }
        }
        Ok(())
    }
    fn reuid(n: &mut Node, ids: &mut Ids) {
        match n {
            Node::Prop(p) => p.uid = ids.alloc(),
            Node::Group(g) => {
                g.uid = ids.alloc();
                g.children.iter_mut().for_each(|c| reuid(c, ids));
            }
        }
    }
    let mut j = j.clone();
    add(&mut j, 0)?;
    let mut n: Node = serde_json::from_value(j).map_err(|e| format!("bad raw node: {e}"))?;
    reuid(&mut n, ids);
    Ok(n)
}

fn raw_group(j: &J, ids: &mut Ids) -> R<PropGroup> {
    match raw_node(j, ids)? {
        Node::Group(g) => Ok(g),
        Node::Prop(_) => Err("this raw entry must be a group".into()),
    }
}

/// Keys of an instance entry that aren't group header fields.
const RESERVED: &[&str] = &["kind", "effect", "name", "enabled", "label", "raw", "contents", "selectors", "props", "threeD", "header"];

/// Header differences of group `o` from `b`: name, eye/fx switch, label and the group kind's
/// data (a mask's mode…; flat unless the key is reserved, then under `header`).
fn header_diff(o: &PropGroup, b: &PropGroup) -> Map<String, J> {
    let mut m = Map::new();
    if o.name != b.name {
        m.insert("name".into(), J::from(o.name.clone()));
    }
    if o.enabled != b.enabled {
        m.insert("enabled".into(), J::from(o.enabled));
    }
    if o.label != b.label {
        m.insert("label".into(), to_json(&o.label));
    }
    if let Some(J::Object(d)) = diff_json(&to_json(&o.kind), &to_json(&b.kind)) {
        let mut header = Map::new();
        for (k, v) in d {
            if RESERVED.contains(&k.as_str()) {
                header.insert(k, v);
            } else {
                m.insert(k, v);
            }
        }
        if !header.is_empty() {
            m.insert("header".into(), J::Object(header));
        }
    }
    m
}

/// Apply [`header_diff`] fields of entry `e` to `g`.
fn apply_header(e: &Map<String, J>, g: &mut PropGroup) -> R<()> {
    if let Some(n) = e.get("name") {
        g.name = n.as_str().ok_or("`name` must be a string")?.to_string();
    }
    if let Some(v) = e.get("enabled") {
        g.enabled = v.as_bool().ok_or("`enabled` must be true or false")?;
    }
    if let Some(v) = e.get("label") {
        g.label = serde_json::from_value(v.clone()).map_err(|err| format!("bad `label`: {err}"))?;
    }
    let mut patch = match e.get("header") {
        Some(J::Object(h)) => h.clone(),
        Some(_) => return Err("`header` must be an object".into()),
        None => Map::new(),
    };
    for (k, v) in e {
        if !RESERVED.contains(&k.as_str()) {
            patch.insert(k.clone(), v.clone());
        }
    }
    if !patch.is_empty() {
        let mut kj = to_json(&g.kind);
        merge_json(&mut kj, &J::Object(patch));
        g.kind = serde_json::from_value(kj).map_err(|err| format!("bad group settings in {}: {err}", g.name))?;
    }
    Ok(())
}

// ---------------------------------------------------------------- values

/// Layer references while writing a recipe: layer id → `"Name"` or `"#n"`.
struct Enc {
    refs: HashMap<u64, String>,
    fr: FrameRate,
    /// Layer ids referenced but not in the recipe.
    missing: Vec<u64>,
}

impl Enc {
    fn layer_ref(&mut self, id: u64) -> J {
        match self.refs.get(&id) {
            Some(r) => J::from(r.clone()),
            None => {
                self.missing.push(id);
                J::Null
            }
        }
    }
}

/// Layer references while building: the recipe's layers in order (`#n`, names).
struct Dec {
    layers: Vec<(String, LayerId)>,
    fr: FrameRate,
}

impl Dec {
    fn layer(&self, j: &J) -> R<Option<LayerId>> {
        let by_index = |i: u64| -> R<Option<LayerId>> {
            usize::try_from(i)
                .ok()
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| self.layers.get(i))
                .map(|l| Some(l.1))
                .ok_or_else(|| format!("no layer #{i} in the recipe"))
        };
        match j {
            J::Null => Ok(None),
            J::Number(n) => by_index(n.as_u64().ok_or("a layer number is 1-based")?),
            J::String(s) => {
                if let Some(i) = s.strip_prefix('#').and_then(|i| i.parse::<u64>().ok()) {
                    return by_index(i);
                }
                self.layers.iter().find(|l| &l.0 == s).map(|l| Some(l.1)).ok_or_else(|| format!("no layer named `{s}` in the recipe"))
            }
            _ => Err("a layer reference is a name, \"#n\" or null".into()),
        }
    }
}

/// Whether a 3D value's z can be left out: a property shown in 2D whose z is the default's
/// everywhere.
fn drop_z(p: &Property, base: &Property) -> bool {
    let KV::Vec3(b) = base.value else { return false };
    let z_ok = |v: &KV| matches!(v, KV::Vec3(a) if r4(a[2]) == r4(b[2]));
    p.shown_dims == 2 && if p.keys.is_empty() { z_ok(&p.value) } else { p.keys.iter().all(|k| z_ok(&k.value)) }
}

/// A dropdown's own menu items (Dropdown Menu Control ▸ Edit) when they aren't the effect's
/// default ones: the recipe carries them, else a value named after them can't be read back.
fn popup_items(p: &Property, base: &Property) -> Option<Vec<String>> {
    match (&p.ui, &base.ui) {
        (ParamUi::Popup { options }, ParamUi::Popup { options: b }) if options != b => Some(options.clone()),
        _ => None,
    }
}

fn popup_label(ui: &ParamUi, i: u32) -> Option<String> {
    let ParamUi::Popup { options } = ui else { return None };
    let o = options.get(usize::try_from(i).ok()?)?;
    (options.iter().filter(|x| *x == o).count() == 1).then(|| o.clone())
}

fn path_enc(p: &ShapePath) -> J {
    let mut j = rounded(&to_json(p));
    if let J::Object(o) = &mut j {
        for k in ["in_tangents", "out_tangents"] {
            let zero =
                o.get(k).and_then(J::as_array).is_some_and(|a| a.iter().all(|t| t.as_array().is_some_and(|t| t.iter().all(|x| x.as_f64() == Some(0.0)))));
            if zero {
                o.remove(k);
            }
        }
    }
    j
}

/// A value as recipe JSON (numbers rounded; `base` is the default the value is compared with).
fn enc_val(v: &KV, base: &KV, p: &Property, dz: bool, enc: &mut Enc) -> J {
    match v {
        KV::Scalar(x) => num(*x),
        KV::Vec2(a) => nums(a),
        KV::Vec3(a) => nums(if dz { &a[..2] } else { a }),
        KV::Color(c) => nums(if r4(c[3]) == 1.0 { &c[..3] } else { c }),
        KV::Bool(b) => J::from(*b),
        KV::Enum(i) => popup_label(&p.ui, *i).map_or_else(|| J::from(*i), J::from),
        KV::Str(s) => J::from(s.clone()),
        KV::Layer(id) => id.map_or(J::Null, |id| enc.layer_ref(id)),
        KV::Path(sp) => path_enc(sp),
        KV::Gradient(g) => rounded(&to_json(g)),
        KV::Text(t) => match base {
            KV::Text(b) => diff_json(&to_json(t), &to_json(b)).unwrap_or_else(|| json!({})),
            _ => rounded(&to_json(t)),
        },
    }
}

fn numbers(j: &J) -> R<Vec<f64>> {
    match j {
        J::Number(n) => n.as_f64().map(|x| vec![x]).ok_or_else(|| "not a number".to_string()),
        J::Bool(b) => Ok(vec![if *b { 1.0 } else { 0.0 }]),
        J::Array(a) => a.iter().map(|x| x.as_f64().filter(|x| x.is_finite()).ok_or_else(|| format!("{x} is not a number"))).collect(),
        _ => Err(format!("expected a number or an array of numbers, got {j}")),
    }
}

/// Recipe JSON back to a value shaped like `base` (missing components come from `base`).
fn dec_val(j: &J, base: &KV, p: &Property, dec: &Dec) -> R<KV> {
    let comps = |j: &J| -> R<Vec<f64>> {
        let mut c = numbers(j)?;
        let dims = base.dims();
        if c.iter().any(|x| !x.is_finite()) {
            return Err("numbers must be finite".into());
        }
        if let [x] = c.as_slice()
            && dims > 1
        {
            c = vec![*x; dims];
        }
        let b = base.components();
        while c.len() < dims {
            c.push(b.get(c.len()).copied().unwrap_or(0.0));
        }
        Ok(c)
    };
    Ok(match base {
        KV::Enum(_) if j.is_string() => {
            let s = j.as_str().unwrap_or_default();
            let ParamUi::Popup { options } = &p.ui else { return Err(format!("`{s}`: this property takes a number")) };
            // An item's name, else "Item N" (1-based, as the default menu names them).
            let nth = || {
                let n = s.get(..5).filter(|w| w.eq_ignore_ascii_case("item "))?;
                let i = s.get(n.len()..)?.trim().parse::<usize>().ok()?.checked_sub(1)?;
                (i < options.len()).then_some(i)
            };
            let i = options.iter().position(|o| o.eq_ignore_ascii_case(s)).or_else(nth).ok_or_else(|| format!("`{s}` isn't one of {}", options.join(", ")))?;
            KV::Enum(u32::try_from(i).unwrap_or(0))
        }
        KV::Scalar(_) | KV::Vec2(_) | KV::Vec3(_) | KV::Enum(_) => base.with_components(&comps(j)?),
        KV::Color(_) => {
            if let Some(s) = j.as_str() {
                let c = effectcraft_color::Rgba::from_hex(s).ok_or_else(|| format!("`{s}` isn't a colour (#rrggbb or [r, g, b, a?] 0–1)"))?;
                KV::Color([c.r as f64, c.g as f64, c.b as f64, c.a as f64])
            } else {
                let mut c = numbers(j)?;
                if c.len() == 3 {
                    c.push(1.0);
                }
                let [r, g, b, a] = c.as_slice() else { return Err("a colour is [r, g, b] or [r, g, b, a] (0–1)".into()) };
                KV::Color([*r, *g, *b, *a])
            }
        }
        KV::Bool(_) => KV::Bool(j.as_bool().or_else(|| j.as_f64().map(|x| x != 0.0)).ok_or("expected true or false")?),
        KV::Str(_) => KV::Str(j.as_str().ok_or("expected a string")?.to_string()),
        KV::Layer(_) => KV::Layer(dec.layer(j)?.map(|l| l.0)),
        KV::Path(_) => {
            let mut b = to_json(&ShapePath::default());
            merge_json(&mut b, j);
            let mut sp: ShapePath = serde_json::from_value(b).map_err(|e| format!("bad path: {e}"))?;
            sp.fit_tangents();
            KV::path(sp)
        }
        KV::Text(t) => {
            if let Some(s) = j.as_str() {
                let mut d = (**t).clone();
                d.set_text(s);
                KV::Text(Box::new(d))
            } else {
                let mut b = to_json(&**t);
                merge_json(&mut b, j);
                KV::Text(Box::new(serde_json::from_value::<TextDoc>(b).map_err(|e| format!("bad text: {e}"))?))
            }
        }
        KV::Gradient(_) => base.coerce_json(j).ok_or("bad gradient: {colors: [[pos, [r, g, b, a]]…], opacities: [[pos, a]…]}")?,
    })
}

// ---------------------------------------------------------------- keyframes

fn eases_json(e: &Eases) -> J {
    J::Array(e.iter().map(|x| json!([num(x.speed), num(x.influence)])).collect())
}

fn easy(e: &Ease) -> bool {
    e.speed == Ease::EASY.speed && e.influence == Ease::EASY.influence
}

/// One side of a key: absent (linear), `"hold"`, `"easy"`, `"bezier"`, `[[speed, influence]…]`
/// (Bezier) or `{interp, ease}`.
fn enc_side(i: Interp, e: &Eases, n: usize) -> Option<J> {
    match i {
        Interp::Linear if e.is_empty() => None,
        Interp::Hold if e.is_empty() => Some(J::from("hold")),
        Interp::Bezier if e.is_empty() => Some(J::from("bezier")),
        Interp::Bezier if e.len() == n && e.iter().all(easy) => Some(J::from("easy")),
        Interp::Bezier => Some(eases_json(e)),
        _ => Some(json!({"interp": to_json(&i), "ease": eases_json(e)})),
    }
}

fn dec_eases(j: &J) -> R<Eases> {
    // 0.3333 is a third (Easy Ease's influence), exactly.
    let snap = |x: f64| {
        if (x - 1.0 / 3.0).abs() < 5e-5 {
            1.0 / 3.0
        } else if (x - 2.0 / 3.0).abs() < 5e-5 {
            2.0 / 3.0
        } else {
            x
        }
    };
    let a = j.as_array().ok_or("eases are [[speed, influence]…]")?;
    a.iter()
        .take(16)
        .map(|p| {
            let c = numbers(p)?;
            match c.as_slice() {
                [s, i] => Ok(Ease { speed: *s, influence: snap(*i) }),
                _ => Err("an ease is [speed, influence]".to_string()),
            }
        })
        .collect::<R<Vec<_>>>()
        .map(Eases::from)
}

fn dec_side(j: Option<&J>, n: usize) -> R<(Interp, Eases)> {
    Ok(match j {
        None | Some(J::Null) => (Interp::Linear, Eases::new()),
        Some(J::String(s)) => match s.as_str() {
            "linear" => (Interp::Linear, Eases::new()),
            "hold" => (Interp::Hold, Eases::new()),
            "bezier" => (Interp::Bezier, Eases::new()),
            "easy" => (Interp::Bezier, Eases::splat(Ease::EASY, n)),
            s => return Err(format!("unknown easing `{s}` (linear, hold, easy, bezier or [[speed, influence]…])")),
        },
        Some(a @ J::Array(_)) => (Interp::Bezier, dec_eases(a)?),
        Some(J::Object(o)) => {
            let i: Interp = serde_json::from_value(o.get("interp").cloned().unwrap_or(J::Null)).map_err(|e| format!("bad `interp`: {e}"))?;
            (i, o.get("ease").map(dec_eases).transpose()?.unwrap_or_else(Eases::new))
        }
        Some(j) => return Err(format!("bad easing {j}")),
    })
}

const KEY_FIELDS: &[&str] = &["time", "value", "in_interp", "out_interp", "in_ease", "out_ease"];

fn enc_key(k: &Keyframe, p: &Property, base: &Property, dz: bool, enc: &mut Enc) -> J {
    let mut o = Map::new();
    o.insert("time".into(), time_enc(k.time, enc.fr));
    o.insert("value".into(), enc_val(&k.value, &base.value, p, dz, enc));
    let n = k.value.dims().max(1);
    if let Some(s) = enc_side(k.in_interp, &k.in_ease, n) {
        o.insert("in".into(), s);
    }
    if let Some(s) = enc_side(k.out_interp, &k.out_ease, n) {
        o.insert("out".into(), s);
    }
    // Spatial tangents, roving, labels…: as stored.
    if let J::Object(m) = to_json(k) {
        for (key, v) in m {
            if !KEY_FIELDS.contains(&key.as_str()) {
                o.insert(key, rounded(&v));
            }
        }
    }
    J::Object(o)
}

fn dec_key(j: &J, base: &Property, dec: &Dec) -> R<Keyframe> {
    let o = j.as_object().ok_or("a keyframe is {time, value, in?, out?}")?;
    let time = time_p(o.get("time").ok_or("a keyframe needs `time` (seconds, layer time)")?, dec.fr, "time")?;
    let value = dec_val(o.get("value").ok_or("a keyframe needs `value`")?, &base.value, base, dec)?;
    let n = value.dims().max(1);
    let (in_interp, in_ease) = dec_side(o.get("in"), n)?;
    let (out_interp, out_ease) = dec_side(o.get("out"), n)?;
    let mut k = Keyframe::new(time, value);
    k.in_interp = in_interp;
    k.out_interp = out_interp;
    k.in_ease = in_ease;
    k.out_ease = out_ease;
    let extra: Map<String, J> =
        o.iter().filter(|(key, _)| !matches!(key.as_str(), "time" | "value" | "in" | "out")).map(|(a, b)| (a.clone(), b.clone())).collect();
    if !extra.is_empty() {
        let mut kj = to_json(&k);
        merge_json(&mut kj, &J::Object(extra));
        k = serde_json::from_value(kj).map_err(|e| format!("bad keyframe: {e}"))?;
    }
    Ok(k)
}

// ---------------------------------------------------------------- properties

/// A property as recipe JSON: its value, or `{keys?, value?, expr?, exprEnabled?, items?}` when
/// it is animated, has an expression or is a dropdown with its own menu items.
fn enc_prop(p: &Property, base: &Property, enc: &mut Enc) -> J {
    let dz = drop_z(p, base);
    let items = popup_items(p, base);
    if p.keys.is_empty() && p.expr.is_none() && items.is_none() {
        return enc_val(&p.value, &base.value, p, dz, enc);
    }
    let mut o = Map::new();
    if p.keys.is_empty() {
        o.insert("value".into(), enc_val(&p.value, &base.value, p, dz, enc));
    } else {
        o.insert("keys".into(), J::Array(p.keys.iter().map(|k| enc_key(k, p, base, dz, enc)).collect()));
    }
    if let Some(e) = &p.expr {
        o.insert("expr".into(), J::from(e.text.clone()));
        if !e.enabled {
            o.insert("exprEnabled".into(), J::from(false));
        }
    }
    if let Some(items) = items {
        o.insert("items".into(), J::from(items));
    }
    J::Object(o)
}

/// `{keys?, value?, expr?, exprEnabled?, items?}` with keys, an expression or menu items (not a
/// value that is an object, like a gradient or a text style).
fn is_wrapper(j: &J) -> bool {
    j.as_object().is_some_and(|o| {
        (o.contains_key("keys") || o.contains_key("expr") || o.contains_key("items"))
            && o.keys().all(|k| matches!(k.as_str(), "keys" | "value" | "expr" | "exprEnabled" | "items"))
    })
}

/// Set `p` (holding its default) from recipe JSON: value, keys and expression all follow it.
fn dec_prop(j: &J, p: &mut Property, dec: &Dec) -> R<()> {
    if !is_wrapper(j) {
        let base = p.clone();
        p.value = dec_val(j, &base.value, &base, dec)?;
        p.keys.clear();
        p.expr = None;
        return Ok(());
    }
    let o = j.as_object().ok_or("expected an object")?;
    // A dropdown's own menu items come first: its value and keys may be named after them.
    if let Some(items) = o.get("items") {
        let items: Vec<String> = items.as_array().ok_or("`items` must be an array of strings")?.iter().filter_map(J::as_str).map(str::to_string).collect();
        if items.is_empty() || items.len() > 10_000 {
            return Err("`items`: a dropdown needs 1 to 10000 menu items".into());
        }
        if !matches!(p.ui, ParamUi::Popup { .. }) {
            return Err("`items`: not a dropdown property".into());
        }
        p.ui = ParamUi::Popup { options: items };
    }
    let base = p.clone();
    p.keys.clear();
    if let Some(v) = o.get("value") {
        p.value = dec_val(v, &base.value, &base, dec)?;
    }
    if let Some(keys) = o.get("keys") {
        let keys = keys.as_array().ok_or("`keys` must be an array")?;
        if keys.len() > 100_000 {
            return Err("too many keyframes".into());
        }
        let mut ks = keys.iter().map(|k| dec_key(k, &base, dec)).collect::<R<Vec<_>>>()?;
        ks.sort_by_key(|k| k.time);
        ks.dedup_by_key(|k| k.time);
        p.keys = ks;
    }
    p.expr = match o.get("expr") {
        Some(e) => Some(Expression {
            text: e.as_str().ok_or("`expr` must be a string")?.to_string(),
            enabled: o.get("exprEnabled").and_then(J::as_bool).unwrap_or(true),
        }),
        None => None,
    };
    Ok(())
}

// ---------------------------------------------------------------- structure: list entries

/// The instance lists a recipe describes item by item.
#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
    /// Shape contents (a shape layer's, or a shape group's).
    Shape,
    Effects,
    Masks,
    /// Text animators.
    Animators,
    /// An animator's selectors.
    Selectors,
}

struct BuildCx<'a> {
    gl: &'a styles::GlobalLight,
    /// The layer's effect space, for effect defaults (centres…).
    layer_size: [f64; 2],
}

/// Kind of an entry (`kind`, or `effect` for effects).
fn entry_kind(e: &Map<String, J>, list: List) -> Option<&str> {
    e.get(if list == List::Effects { "effect" } else { "kind" }).and_then(J::as_str)
}

const SELECTORS: &[(&str, &str, &str)] = &[
    ("range", "rangeSelector", "Range Selector"),
    ("wiggly", "wigglySelector", "Wiggly Selector"),
    ("expression", "expressionSelector", "Expression Selector"),
];

/// Text animator property kinds (`layer.addTextAnimator` kinds) of an animator's properties,
/// in order; companions (Skew Axis, Tracking Type…) come with their kind.
fn animator_kinds(props: &PropGroup) -> (Vec<String>, bool) {
    let mut kinds: Vec<String> = vec![];
    let mut three_d = false;
    for p in props.props() {
        let m = p.match_id.as_str();
        if p.shown_dims == 3 {
            three_d = true;
        }
        let kind = match m {
            "skewAxis" | "trackingType" | "characterAlignment" | "characterRange" => continue,
            "rotationX" | "rotationY" => {
                three_d = true;
                continue;
            }
            m if build::GRADIENT_FILL_PROPS.contains(&m) => "gradientFill",
            m => m,
        };
        if kind == "gradientFill" && kinds.last().is_some_and(|k| k == "gradientFill") {
            continue;
        }
        kinds.push(kind.to_string());
    }
    (kinds, three_d)
}

/// Describe item `g` of `list` without its header (nested lists extracted, verified).
fn describe(list: List, g: &PropGroup, cx: &BuildCx, depth: usize) -> Map<String, J> {
    let mut e = Map::new();
    match list {
        List::Shape => {
            let kind = if g.match_id == "star" && g.get("innerRadius").is_none() { "polygon" } else { g.match_id.as_str() };
            e.insert("kind".into(), J::from(kind));
            if build::SHAPE_SURFACES.iter().any(|(m, _)| *m == g.match_id) {
                e.insert("props".into(), J::Array(g.children.iter().map(|c| J::from(c.match_id())).collect()));
            }
            if g.match_id == "group"
                && let Some(c) = g.sub("contents")
            {
                e.insert("contents".into(), J::Array(extract_list(List::Shape, c, cx, depth + 1)));
            }
        }
        List::Effects => {
            if let GroupKind::Effect { effect } = &g.kind {
                e.insert("effect".into(), J::from(effect.clone()));
            }
        }
        List::Masks => {}
        List::Animators => {
            e.insert("kind".into(), J::from("animator"));
            if let Some(sels) = g.sub("selectors") {
                let s = extract_list(List::Selectors, sels, cx, depth + 1);
                if s != [json!({"kind": "range"})] {
                    e.insert("selectors".into(), J::Array(s));
                }
            }
            if let Some(props) = g.sub("properties") {
                let (kinds, three_d) = animator_kinds(props);
                e.insert("props".into(), J::Array(kinds.into_iter().map(J::from).collect()));
                if three_d {
                    e.insert("threeD".into(), J::from(true));
                }
            }
        }
        List::Selectors => {
            let kind = SELECTORS.iter().find(|s| s.1 == g.match_id).map_or(g.match_id.as_str(), |s| s.0);
            e.insert("kind".into(), J::from(kind));
        }
    }
    e
}

/// Build item `e` of `list`: the factory default for its kind, its nested lists, its header.
/// `index` is its 1-based place in the list, `same` how many items of its kind come before it.
fn build_item(list: List, e: &J, index: usize, same: usize, ids: &mut Ids, cx: &BuildCx, depth: usize) -> R<PropGroup> {
    if depth > MAX_TREE_DEPTH {
        return Err("recipe nested too deep".into());
    }
    let owned;
    let e = match e {
        J::String(s) => {
            owned = json!({"kind": s});
            &owned
        }
        e => e,
    };
    let em = e.as_object().ok_or("a list entry must be an object")?;
    if let Some(raw) = em.get("raw") {
        return raw_group(raw, ids);
    }
    let kind = entry_kind(em, list).unwrap_or_default();
    let mut g = match list {
        List::Shape => {
            if let Some((_, name)) = build::SHAPE_SURFACES.iter().find(|(m, _)| *m == kind) {
                let mut g = build::shape_surface(ids, kind).ok_or_else(|| format!("unknown surface `{name}`"))?;
                for p in em.get("props").and_then(J::as_array).into_iter().flatten() {
                    let pm = p.as_str().unwrap_or_default();
                    g.children.push(build::shape_surface_prop(ids, pm).ok_or_else(|| format!("unknown {name} property `{pm}`"))?.into());
                }
                g
            } else {
                let mut g = super::layer::new_shape_item(ids, kind).ok_or_else(|| format!("unknown shape item kind `{kind}`"))?;
                if kind == "group"
                    && let Some(items) = em.get("contents")
                {
                    let built = build_list(List::Shape, items, ids, cx, depth + 1)?;
                    let c = g.sub_mut("contents").ok_or("shape group without contents")?;
                    c.children = built.into_iter().map(Node::from).collect();
                }
                g
            }
        }
        List::Effects => {
            let spec = effectcraft_effects::find(kind)
                .or_else(|| effectcraft_effects::lookup(kind))
                .ok_or_else(|| super::effect::unknown_effect(if kind.is_empty() { "(missing `effect`)" } else { kind }))?;
            let name = if same == 0 { spec.name.to_string() } else { format!("{} {}", spec.name, same + 1) };
            effectcraft_effects::instantiate(spec, ids, &name, cx.layer_size)
        }
        List::Masks => {
            let color = build::MASK_COLORS.get(index.saturating_sub(1) % build::MASK_COLORS.len()).copied().unwrap_or([0xb1, 0xb4, 0x4f]);
            build::mask(ids, &format!("Mask {index}"), ShapePath::default(), MaskMode::Add, color)
        }
        List::Animators => {
            let three_d = em.get("threeD").and_then(J::as_bool).unwrap_or(false);
            let mut props = vec![];
            for k in em.get("props").and_then(J::as_array).into_iter().flatten() {
                let k = k.as_str().ok_or("animator `props` are property kinds (position, opacity, fillColor…)")?;
                let mut more = build::text_anim_props(ids, k, three_d);
                if more.is_empty() {
                    return Err(format!("unknown text animator property `{k}`"));
                }
                props.append(&mut more);
            }
            let mut g = build::text_animator(ids, &format!("Animator {index}"), props);
            if let Some(sels) = em.get("selectors") {
                let built = build_list(List::Selectors, sels, ids, cx, depth + 1)?;
                let s = g.sub_mut("selectors").ok_or("animator without selectors")?;
                s.children = built.into_iter().map(Node::from).collect();
            }
            g
        }
        List::Selectors => {
            let (k, _, label) = SELECTORS.iter().find(|s| s.0 == kind).ok_or_else(|| format!("unknown selector kind `{kind}` (range, wiggly, expression)"))?;
            build::text_selector(ids, k, &format!("{label} {}", same + 1)).ok_or("unknown selector")?
        }
    };
    let mut header = em.clone();
    for k in ["kind", "effect", "props", "contents", "selectors", "threeD"] {
        header.remove(k);
    }
    apply_header(&header, &mut g)?;
    Ok(g)
}

fn build_list(list: List, entries: &J, ids: &mut Ids, cx: &BuildCx, depth: usize) -> R<Vec<PropGroup>> {
    let entries = entries.as_array().ok_or("a list must be an array")?;
    if entries.len() > MAX_ITEMS {
        return Err("too many items".into());
    }
    let mut out = vec![];
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        let kind = match e {
            J::String(s) => s.clone(),
            J::Object(o) => entry_kind(o, list).unwrap_or_default().to_string(),
            _ => String::new(),
        };
        let same = seen.entry(kind).or_insert(0);
        out.push(build_item(list, e, i + 1, *same, ids, cx, depth)?);
        *same += 1;
    }
    Ok(out)
}

/// The entries of list group `parent` (its children): each described and checked by building
/// it again; what doesn't come back the same is a `raw` entry.
fn extract_list(list: List, parent: &PropGroup, cx: &BuildCx, depth: usize) -> Vec<J> {
    let mut out: Vec<J> = vec![];
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, child) in parent.children.iter().enumerate() {
        let raw = || json!({"kind": child.match_id(), "raw": raw_json(child)});
        let Node::Group(g) = child else {
            out.push(raw());
            continue;
        };
        if depth > MAX_TREE_DEPTH {
            out.push(raw());
            continue;
        }
        let mut e = describe(list, g, cx, depth);
        let kind = entry_kind(&e, list).unwrap_or_default().to_string();
        let same = seen.get(&kind).copied().unwrap_or(0);
        let mut tmp = 1;
        let mut ids = Ids(&mut tmp);
        let entry = match build_item(list, &J::Object(e.clone()), i + 1, same, &mut ids, cx, depth) {
            Ok(bare) => {
                e.extend(header_diff(g, &bare));
                let ej = J::Object(e);
                match build_item(list, &ej, i + 1, same, &mut ids, cx, depth) {
                    Ok(built) if same_group(&built, g, depth) => ej,
                    _ => raw(),
                }
            }
            Err(_) => raw(),
        };
        *seen.entry(kind).or_insert(0) += 1;
        out.push(entry);
    }
    out
}

fn style_entry(e: &J) -> Option<&str> {
    e.as_str().or_else(|| e.get("kind").and_then(J::as_str))
}

/// The Layer Styles group of `entries` (style ids, or `{kind, enabled?…}`).
fn build_styles(entries: &J, ids: &mut Ids, gl: &styles::GlobalLight) -> R<PropGroup> {
    let mut g = styles::layer_styles(ids, gl);
    for e in entries.as_array().ok_or("`styles` must be an array")? {
        let id = style_entry(e).ok_or("a style is its id (dropShadow, innerShadow…) or {kind, …}")?;
        let id = styles::style_id(id).ok_or_else(|| format!("unknown layer style `{id}`"))?;
        let mut s = styles::style(ids, id).ok_or_else(|| format!("unknown layer style `{id}`"))?;
        if let J::Object(o) = e {
            let mut h = o.clone();
            h.remove("kind");
            apply_header(&h, &mut s)?;
        }
        g.children.push(s.into());
    }
    Ok(g)
}

fn extract_styles(ls: &PropGroup, gl: &styles::GlobalLight) -> Option<J> {
    let mut out = vec![];
    for c in &ls.children {
        let Node::Group(g) = c else { return None };
        if g.match_id == styles::BLENDING {
            continue;
        }
        let mut tmp = 1;
        let bare = styles::style(&mut Ids(&mut tmp), &g.match_id)?;
        let h = header_diff(g, &bare);
        out.push(if h.is_empty() {
            J::from(g.match_id.clone())
        } else {
            let mut o = Map::new();
            o.insert("kind".into(), J::from(g.match_id.clone()));
            o.extend(h);
            J::Object(o)
        });
    }
    let out = J::Array(out);
    let mut tmp = 1;
    let built = build_styles(&out, &mut Ids(&mut tmp), gl).ok()?;
    same_group(&built, ls, 0).then_some(out)
}

// ---------------------------------------------------------------- layers

/// The layer's type in a recipe.
fn type_name(src: &LayerSource) -> &'static str {
    match src {
        LayerSource::Footage { .. } => "footage",
        LayerSource::Comp { .. } => "precomp",
        LayerSource::Solid { .. } => "solid",
        LayerSource::Text => "text",
        LayerSource::Shape => "shape",
        LayerSource::Null => "null",
        LayerSource::Camera => "camera",
        LayerSource::Light { .. } => "light",
        LayerSource::Model { .. } => "model",
        LayerSource::Primitive { .. } => "primitive",
    }
}

/// Size `build::layer` is given for a source (as the New Layer commands do).
fn source_size(proj: &Project, src: &LayerSource, comp: &Comp) -> (u32, u32) {
    src.item().and_then(|i| proj.item(i)).and_then(|i| i.dimensions()).unwrap_or((comp.width, comp.height))
}

/// A fresh layer of `source` with the structure the recipe layer `spec` lists (contents,
/// effects, masks, styles, animators, root drops and extras): what its values are compared with.
fn assemble(proj: &mut Project, comp: &Comp, source: LayerSource, spec: &Map<String, J>) -> R<Layer> {
    let name = spec.get("name").and_then(J::as_str).unwrap_or("Layer");
    let size = source_size(proj, &source, comp);
    let mut l = build::layer(proj, comp, name, source, size, None);
    l.name = name.to_string();
    if comp.renderer == Renderer::Advanced3D {
        super::model3d::add_geometry_options(&mut proj.next_id, &mut l);
    }
    let cx = BuildCx { gl: &comp.global_light, layer_size: effectcraft_render::effect_bounds(proj, comp, &l).0 };
    let mut ids = Ids(&mut proj.next_id);
    let what = |k: &str| format!("a {} layer has no {k}", type_name(&l.source));
    for (key, list, path) in [
        ("contents", List::Shape, "contents"),
        ("effects", List::Effects, "effects"),
        ("masks", List::Masks, "masks"),
        ("animators", List::Animators, "text/animators"),
    ] {
        if let Some(entries) = spec.get(key) {
            let built = build_list(list, entries, &mut ids, &cx, 0).map_err(|e| format!("{key}: {e}"))?;
            let g = l.props.group_mut(path).ok_or_else(|| what(key))?;
            g.children = built.into_iter().map(Node::from).collect();
        }
    }
    if let Some(entries) = spec.get("styles") {
        let g = build_styles(entries, &mut ids, cx.gl).map_err(|e| format!("styles: {e}"))?;
        let root = &mut l.props.children;
        root.retain(|c| c.match_id() != styles::GROUP);
        let at = root.iter().position(|c| c.match_id() == "transform").map_or(root.len(), |i| i + 1);
        root.insert(at, g.into());
    }
    // Root groups the layer has that the recipe's layer doesn't, and the other way round.
    for d in spec.get("drop").and_then(J::as_array).into_iter().flatten() {
        let m = d.as_str().ok_or("`drop` lists match ids")?;
        let i = l.props.children.iter().position(|c| c.match_id() == m).ok_or_else(|| format!("`drop`: the layer has no `{m}`"))?;
        l.props.children.remove(i);
    }
    for x in spec.get("extra").and_then(J::as_array).into_iter().flatten() {
        let at = x.get("at").and_then(J::as_u64).and_then(|a| usize::try_from(a).ok()).ok_or("`extra` entries are {at, raw}")?;
        let node = raw_node(x.get("raw").ok_or("`extra` entries are {at, raw}")?, &mut ids)?;
        let at = at.min(l.props.children.len());
        l.props.children.insert(at, node);
    }
    Ok(l)
}

/// Group header overrides and property values (`groups`, `props`, `text`) onto `l`.
fn apply_values(l: &mut Layer, spec: &Map<String, J>, dec: &Dec) -> R<()> {
    if let Some(groups) = spec.get("groups") {
        for (path, h) in groups.as_object().ok_or("`groups` must be an object {path: {name?, enabled?…}}")? {
            let h = h.as_object().ok_or_else(|| format!("groups/{path}: expected an object"))?;
            let g = l.props.group_mut(path).ok_or_else(|| format!("no property group `{path}`"))?;
            apply_header(h, g).map_err(|e| format!("{path}: {e}"))?;
        }
    }
    let text = spec.get("text").map(|t| ("text/sourceText".to_string(), t.clone()));
    let props = spec.get("props").map(|p| p.as_object().ok_or("`props` must be an object {path: value}")).transpose()?;
    for (path, j) in props.into_iter().flatten().map(|(k, v)| (k.clone(), v.clone())).chain(text) {
        let p = l.props.prop_mut(&path).ok_or_else(|| format!("no property `{path}` (paths as in `effectcraft-cli props`)"))?;
        dec_prop(&j, p, dec).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}

/// Layer fields a recipe sets by name (the rest of a layer's top-level keys are its stored
/// fields, e.g. `label`, `stretch`, `markers`).
const LAYER_KEYS: &[&str] = &[
    "name",
    "type",
    "color",
    "size",
    "pixelAspect",
    "light",
    "primitive",
    "file",
    "footage",
    "comp",
    "contents",
    "effects",
    "masks",
    "styles",
    "animators",
    "drop",
    "extra",
    "groups",
    "props",
    "text",
    "start",
    "in",
    "out",
    "parent",
    "trackMatte",
    "blendMode",
    "switches",
];
/// Stored layer fields compared generically (the others have their own recipe keys).
const LAYER_SKIP: &[&str] = &["props", "id", "name", "source", "start_time", "in_point", "out_point", "parent", "track_matte", "blend_mode", "switches"];

fn layer_fields(l: &Layer) -> J {
    let mut j = to_json(&Layer { props: PropGroup::new(0, "", ""), ..l.clone() });
    if let J::Object(o) = &mut j {
        for k in LAYER_SKIP {
            o.remove(*k);
        }
    }
    j
}

fn layer_meta(l: &Layer, skel: &Layer, enc: &mut Enc) -> Map<String, J> {
    let mut m = Map::new();
    for (k, a, b) in [("start", l.start_time, skel.start_time), ("in", l.in_point, skel.in_point), ("out", l.out_point, skel.out_point)] {
        if a != b {
            m.insert(k.into(), time_enc(a, enc.fr));
        }
    }
    if let Some(p) = l.parent {
        m.insert("parent".into(), enc.layer_ref(p.0));
    }
    if let Some(tm) = &l.track_matte {
        m.insert("trackMatte".into(), json!({"layer": enc.layer_ref(tm.layer.0), "kind": to_json(&tm.kind)}));
    }
    if l.blend_mode != skel.blend_mode {
        m.insert("blendMode".into(), to_json(&l.blend_mode));
    }
    if let Some(d) = diff_json(&to_json(&l.switches), &to_json(&skel.switches)) {
        m.insert("switches".into(), d);
    }
    if let Some(J::Object(d)) = diff_json(&layer_fields(l), &layer_fields(skel)) {
        m.extend(d);
    }
    m
}

fn apply_meta(l: &mut Layer, spec: &Map<String, J>, dec: &Dec) -> R<()> {
    let fields: Map<String, J> = spec.iter().filter(|(k, _)| !LAYER_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    if !fields.is_empty() {
        let props = std::mem::replace(&mut l.props, PropGroup::new(0, "", ""));
        let mut j = to_json(&*l);
        let known = j.as_object().map(|o| o.keys().cloned().collect::<HashSet<_>>()).unwrap_or_default();
        if let Some(k) = fields.keys().find(|k| !known.contains(*k) && !matches!(k.as_str(), "markers_locked" | "environment" | "environment_background")) {
            l.props = props;
            return Err(format!("unknown layer key `{k}`"));
        }
        merge_json(&mut j, &J::Object(fields));
        let r = serde_json::from_value::<Layer>(j);
        let mut nl = match r {
            Ok(nl) => nl,
            Err(e) => {
                l.props = props;
                return Err(format!("bad layer settings: {e}"));
            }
        };
        nl.props = props;
        *l = nl;
    }
    let fr = dec.fr;
    if let Some(v) = spec.get("start") {
        l.start_time = time_p(v, fr, "start")?;
    }
    if let Some(v) = spec.get("in") {
        l.in_point = time_p(v, fr, "in")?;
    }
    if let Some(v) = spec.get("out") {
        l.out_point = time_p(v, fr, "out")?;
    }
    if let Some(v) = spec.get("blendMode") {
        l.blend_mode = serde_json::from_value::<BlendMode>(v.clone()).map_err(|e| format!("bad `blendMode`: {e}"))?;
    }
    if let Some(v) = spec.get("switches") {
        let mut j = to_json(&l.switches);
        merge_json(&mut j, v);
        l.switches = serde_json::from_value::<Switches>(j).map_err(|e| format!("bad `switches`: {e}"))?;
    }
    if let Some(v) = spec.get("parent") {
        l.parent = dec.layer(v).map_err(|e| format!("parent: {e}"))?.filter(|p| *p != l.id);
    }
    if let Some(v) = spec.get("trackMatte") {
        l.track_matte = match v {
            J::Null => None,
            v => {
                let layer = dec.layer(v.get("layer").unwrap_or(&J::Null)).map_err(|e| format!("trackMatte: {e}"))?;
                let kind =
                    v.get("kind").map(|k| serde_json::from_value(k.clone())).transpose().map_err(|e| format!("bad trackMatte kind: {e}"))?.unwrap_or_default();
                layer.filter(|m| *m != l.id).map(|layer| TrackMatte { layer, kind })
            }
        };
    }
    Ok(())
}

// ---------------------------------------------------------------- comp settings

const COMP_SPECIAL: &[&str] = &["width", "height", "frame_rate", "duration", "background", "work_area", "layers", "essential", "resolution"];
const COMP_KEYS: &[&str] = &["name", "width", "height", "frameRate", "duration", "background", "workArea"];

fn comp_fields(c: &Comp) -> J {
    let mut j = to_json(&Comp { layers: Vec::<Layer>::new().into(), ..c.clone() });
    if let J::Object(o) = &mut j {
        for k in COMP_SPECIAL {
            o.remove(*k);
        }
    }
    j
}

fn comp_meta(c: &Comp, name: &str) -> J {
    let fr = c.frame_rate;
    let mut m = Map::new();
    m.insert("name".into(), J::from(name));
    m.insert("width".into(), J::from(c.width));
    m.insert("height".into(), J::from(c.height));
    m.insert("frameRate".into(), rate_enc(fr));
    m.insert("duration".into(), time_enc(c.duration, fr));
    if c.background != [0.0; 3] {
        m.insert("background".into(), nums(&c.background.map(f64::from)));
    }
    if c.work_area != (Tick::ZERO, c.duration) {
        m.insert("workArea".into(), json!([time_enc(c.work_area.0, fr), time_enc(c.work_area.1, fr)]));
    }
    let base = Comp::new(c.width, c.height, fr, c.duration);
    if let Some(J::Object(d)) = diff_json(&comp_fields(c), &comp_fields(&base)) {
        m.extend(d);
    }
    J::Object(m)
}

fn comp_from_json(j: &Map<String, J>) -> R<Comp> {
    let dim = |k: &str| j.get(k).and_then(J::as_u64).map(limits::size).ok_or_else(|| format!("comp `{k}` must be a whole number of pixels"));
    let (w, h) = (dim("width")?, dim("height")?);
    let fr = rate_dec(j.get("frameRate").unwrap_or(&J::from(30)))?;
    let dur = time_p(j.get("duration").ok_or("comp `duration` (seconds) is missing")?, fr, "duration")?;
    if dur <= Tick::ZERO {
        return Err("comp `duration` must be more than 0".into());
    }
    let mut c = Comp::new(w, h, fr, dur);
    let fields: Map<String, J> = j.iter().filter(|(k, _)| !COMP_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    if !fields.is_empty() {
        let mut cj = to_json(&c);
        let known = cj.as_object().map(|o| o.keys().cloned().collect::<HashSet<_>>()).unwrap_or_default();
        if let Some(k) = fields.keys().find(|k| !known.contains(*k) && !matches!(k.as_str(), "guides" | "speech")) {
            return Err(format!("unknown comp key `{k}`"));
        }
        merge_json(&mut cj, &J::Object(fields));
        c = serde_json::from_value(cj).map_err(|e| format!("bad comp settings: {e}"))?;
    }
    if let Some(bg) = j.get("background") {
        let v = numbers(bg)?;
        let g = |i: usize| v.get(i).copied().unwrap_or(0.0) as f32;
        c.background = [g(0), g(1), g(2)];
    }
    if let Some(wa) = j.get("workArea") {
        let Some([a, b]) = wa.as_array().map(Vec::as_slice) else { return Err("`workArea` is [start, end] in seconds".into()) };
        let (s, e) = (time_p(a, fr, "workArea")?, time_p(b, fr, "workArea")?);
        c.work_area = (s.clamp(Tick::ZERO, c.duration), e.clamp(Tick::ZERO, c.duration));
    }
    Ok(c)
}

// ---------------------------------------------------------------- extraction

struct Extract<'a> {
    project: &'a Project,
    /// A copy of the project the skeleton layers are built in (ids, items).
    scratch: Project,
    include_defaults: bool,
    unsupported: Vec<J>,
    /// Nested comps, dependencies first.
    precomps: Vec<J>,
    keys: HashMap<ItemId, String>,
    busy: HashSet<ItemId>,
}

impl Extract<'_> {
    fn unsupported(&mut self, comp: &str, layer: Option<&str>, what: &str, why: &str) {
        let mut o = Map::new();
        o.insert("comp".into(), J::from(comp));
        if let Some(l) = layer {
            o.insert("layer".into(), J::from(l));
        }
        o.insert("what".into(), J::from(what));
        o.insert("why".into(), J::from(why));
        self.unsupported.push(J::Object(o));
    }

    /// The precomp key (unique comp name) of nested comp `item`, extracting it first.
    fn precomp(&mut self, item: ItemId, depth: usize) -> R<String> {
        if let Some(k) = self.keys.get(&item) {
            return Ok(k.clone());
        }
        if depth >= MAX_PRECOMP_DEPTH || self.busy.contains(&item) {
            return Err(format!("nested more than {MAX_PRECOMP_DEPTH} comps deep (or in a cycle)"));
        }
        let name = self.project.item(item).map(|i| i.name.clone()).ok_or("its composition is missing")?;
        let mut key = name.clone();
        let mut n = 2;
        while self.keys.values().any(|k| *k == key) {
            key = format!("{name} {n}");
            n += 1;
        }
        self.busy.insert(item);
        let r = self.comp(item, &key, None, depth + 1);
        self.busy.remove(&item);
        let r = r?;
        self.keys.insert(item, key.clone());
        self.precomps.push(r);
        Ok(key)
    }

    fn comp(&mut self, cid: ItemId, name: &str, only: Option<&[LayerId]>, depth: usize) -> R<J> {
        let comp = self.project.comp_arc(cid).ok_or("no such composition")?;
        let chosen: Vec<&Layer> = comp.layers.iter().filter(|l| only.is_none_or(|o| o.contains(&l.id))).collect();
        // References: a unique name, else `#n` (place in the recipe's layer list).
        let mut refs = HashMap::new();
        for (i, l) in chosen.iter().enumerate() {
            let unique = chosen.iter().filter(|o| o.name == l.name).count() == 1 && !l.name.starts_with('#');
            refs.insert(l.id.0, if unique { l.name.clone() } else { format!("#{}", i + 1) });
        }
        let mut enc = Enc { refs, fr: comp.frame_rate, missing: vec![] };
        if comp.essential.is_some() {
            self.unsupported(name, None, "Essential Graphics", "its controls point at property uids, which a rebuilt comp doesn't have; publish them again");
        }
        let mut layers = vec![];
        for l in &chosen {
            match self.layer(&comp, name, l, &mut enc, depth) {
                Ok(j) => layers.push(j),
                Err(why) => self.unsupported(name, Some(&l.name), "layer", &why),
            }
        }
        enc.missing.sort_unstable();
        enc.missing.dedup();
        for id in enc.missing {
            let what = comp.layer(LayerId(id)).map_or_else(|| format!("layer id {id}"), |l| format!("layer `{}`", l.name));
            self.unsupported(name, None, &format!("reference to {what}"), "the layer isn't in the recipe (parent, track matte or layer parameter left empty)");
        }
        Ok(json!({"comp": comp_meta(&comp, name), "layers": layers}))
    }

    fn layer(&mut self, comp: &Comp, comp_name: &str, l: &Layer, enc: &mut Enc, depth: usize) -> R<J> {
        let mut spec = Map::new();
        spec.insert("name".into(), J::from(l.name.clone()));
        spec.insert("type".into(), J::from(type_name(&l.source)));
        match &l.source {
            LayerSource::Light { kind } => {
                spec.insert("light".into(), to_json(kind));
            }
            LayerSource::Primitive { kind } => {
                spec.insert("primitive".into(), to_json(kind));
            }
            LayerSource::Solid { item } => {
                let Some(ItemKind::Solid(s)) = self.project.item(*item).map(|i| &i.kind) else { return Err("its solid is missing".into()) };
                spec.insert("color".into(), nums(&s.color.map(f64::from)));
                spec.insert("size".into(), json!([s.width, s.height]));
                if s.pixel_aspect != 1.0 {
                    spec.insert("pixelAspect".into(), num(s.pixel_aspect));
                }
            }
            LayerSource::Footage { item } | LayerSource::Model { item } => {
                let it = self.project.item(*item).ok_or("its footage is missing")?;
                let ItemKind::Footage(f) = &it.kind else { return Err("its footage is missing".into()) };
                spec.insert("file".into(), J::from(f.path.clone()));
                let strip = |j: J| match j {
                    J::Object(mut o) => {
                        o.remove("path");
                        J::Object(o)
                    }
                    j => j,
                };
                if let Some(d) = diff_json(&strip(to_json(f)), &strip(to_json(&Footage::default()))) {
                    spec.insert("footage".into(), d);
                }
                if !std::path::Path::new(&f.path).exists() {
                    self.unsupported(
                        comp_name,
                        Some(&l.name),
                        "footage file",
                        &format!("{} isn't on this machine: the rebuilt layer is offline until it is relinked", f.path),
                    );
                }
                if it.proxy.is_some() {
                    self.unsupported(comp_name, Some(&l.name), "proxy", "the footage's proxy isn't part of a recipe");
                }
            }
            LayerSource::Comp { item } => {
                let key = self.precomp(*item, depth)?;
                spec.insert("comp".into(), J::from(key));
            }
            _ => {}
        }
        let source = l.source.clone();
        let bare = assemble(&mut self.scratch, comp, source.clone(), &spec)?;
        // The lists, each item checked by building it again.
        let cx = BuildCx { gl: &comp.global_light, layer_size: effectcraft_render::effect_bounds(self.project, comp, l).0 };
        for (key, list, path) in [
            ("contents", List::Shape, "contents"),
            ("effects", List::Effects, "effects"),
            ("masks", List::Masks, "masks"),
            ("animators", List::Animators, "text/animators"),
        ] {
            if let (Some(g), Some(_)) = (l.props.group(path), bare.props.group(path))
                && !g.children.is_empty()
            {
                spec.insert(key.into(), J::Array(extract_list(list, g, &cx, 0)));
            }
        }
        if let Some(ls) = l.props.sub(styles::GROUP)
            && let Some(st) = extract_styles(ls, &comp.global_light)
        {
            spec.insert("styles".into(), st);
        }
        // Root groups: drop what the skeleton has that the layer hasn't, keep the layer's own.
        let skel = assemble(&mut self.scratch, comp, source.clone(), &spec)?;
        let (drop, extra) = reconcile(&l.props, &skel.props);
        if !drop.is_empty() {
            spec.insert("drop".into(), J::Array(drop.into_iter().map(J::from).collect()));
        }
        if !extra.is_empty() {
            spec.insert("extra".into(), J::Array(extra));
        }
        let mut skel = assemble(&mut self.scratch, comp, source.clone(), &spec)?;
        if !same_group(&skel.props, &l.props, 0) {
            // Last resort: every root group as it is.
            spec.insert("drop".into(), J::Array(skel.props.children.iter().map(|c| J::from(c.match_id())).collect()));
            spec.insert("extra".into(), J::Array(l.props.children.iter().enumerate().map(|(i, c)| json!({"at": i, "raw": raw_json(c)})).collect()));
            skel = assemble(&mut self.scratch, comp, source, &spec)?;
            if !same_group(&skel.props, &l.props, 0) {
                return Err("its property tree can't be rebuilt".into());
            }
        }
        if let Some(e) = l.props.sub(effectcraft_project::essential::GROUP)
            && matches!(&e.kind, GroupKind::Essential { overridden } if !overridden.is_empty())
        {
            self.unsupported(comp_name, Some(&l.name), "Essential Properties overrides", "they point at property uids of the nested comp");
        }
        // Values: what differs from the skeleton.
        let (mut props, mut groups) = (Map::new(), Map::new());
        walk(&l.props, &skel.props, "", enc, self.include_defaults, &mut props, &mut groups, 0);
        if let Some(t) = props.get("text/sourceText").filter(|t| !is_wrapper(t)).cloned() {
            props.remove("text/sourceText");
            if t != json!({}) {
                spec.insert("text".into(), t);
            }
        }
        if !groups.is_empty() {
            spec.insert("groups".into(), J::Object(groups));
        }
        if !props.is_empty() {
            spec.insert("props".into(), J::Object(props));
        }
        spec.extend(layer_meta(l, &skel, enc));
        Ok(J::Object(spec))
    }
}

/// Root children of `skel` the layer doesn't have (by match id), and the layer's root children
/// the skeleton doesn't have (`{at, raw}`).
fn reconcile(orig: &PropGroup, skel: &PropGroup) -> (Vec<String>, Vec<J>) {
    let mut used = vec![false; skel.children.len()];
    let mut extra = vec![];
    for (i, o) in orig.children.iter().enumerate() {
        let hit =
            skel.children.iter().enumerate().position(|(j, s)| !used.get(j).copied().unwrap_or(true) && s.match_id() == o.match_id() && same_shape(s, o, 1));
        match hit.and_then(|j| used.get_mut(j)) {
            Some(u) => *u = true,
            None => extra.push(json!({"at": i, "raw": raw_json(o)})),
        }
    }
    let drop = skel.children.iter().zip(&used).filter(|(_, u)| !**u).map(|(c, _)| c.match_id().to_string()).collect();
    (drop, extra)
}

/// Properties of `o` that differ from the skeleton `s` (same shape) by path, and group headers
/// that differ.
fn walk(o: &PropGroup, s: &PropGroup, path: &str, enc: &mut Enc, all: bool, props: &mut Map<String, J>, groups: &mut Map<String, J>, depth: usize) {
    if depth > MAX_TREE_DEPTH {
        return;
    }
    for (i, (oc, sc)) in o.children.iter().zip(&s.children).enumerate() {
        let seg = super::query::segment(o, i);
        let cpath = if path.is_empty() { seg } else { format!("{path}/{seg}") };
        match (oc, sc) {
            (Node::Prop(op), Node::Prop(sp)) => {
                let e = enc_prop(op, sp, enc);
                if all || e != enc_prop(sp, sp, enc) {
                    props.insert(cpath, e);
                }
            }
            (Node::Group(og), Node::Group(sg)) => {
                let h = header_diff(og, sg);
                if !h.is_empty() {
                    groups.insert(cpath.clone(), J::Object(h));
                }
                walk(og, sg, &cpath, enc, all, props, groups, depth + 1);
            }
            _ => {}
        }
    }
}

/// The recipe of comp `cid` (`only`: just these layers).
pub fn recipe(project: &Project, cid: ItemId, only: Option<&[LayerId]>, include_defaults: bool) -> R<J> {
    let name = project.item(cid).map(|i| i.name.clone()).ok_or("no such composition")?;
    let mut x = Extract {
        project,
        scratch: project.clone(),
        include_defaults,
        unsupported: vec![],
        precomps: vec![],
        keys: HashMap::new(),
        busy: HashSet::from([cid]),
    };
    let body = x.comp(cid, &name, only, 0)?;
    let mut out = Map::new();
    out.insert("format".into(), J::from(FORMAT));
    out.insert("version".into(), J::from(VERSION));
    if let J::Object(b) = body {
        out.extend(b);
    }
    if !x.precomps.is_empty() {
        out.insert("precomps".into(), J::Array(x.precomps));
    }
    if !x.unsupported.is_empty() {
        out.insert("unsupported".into(), J::Array(x.unsupported));
    }
    Ok(J::Object(out))
}

// ---------------------------------------------------------------- building

fn make_source(proj: &mut Project, spec: &Map<String, J>, comp: &Comp, precomps: &HashMap<String, ItemId>) -> R<LayerSource> {
    let name = spec.get("name").and_then(J::as_str).unwrap_or("Layer");
    let ty = spec.get("type").and_then(J::as_str).ok_or_else(|| format!("layer `{name}`: missing `type` (shape, solid, text, null, precomp, footage…)"))?;
    let kind_of = |k: &str| spec.get(k).cloned().unwrap_or(J::Null);
    Ok(match ty {
        "shape" => LayerSource::Shape,
        "text" => LayerSource::Text,
        "null" => LayerSource::Null,
        "camera" => LayerSource::Camera,
        "light" => LayerSource::Light { kind: serde_json::from_value(kind_of("light")).unwrap_or_default() },
        "primitive" => LayerSource::Primitive { kind: serde_json::from_value(kind_of("primitive")).map_err(|e| format!("bad `primitive`: {e}"))? },
        "solid" => {
            let color = spec.get("color").map(numbers).transpose()?.unwrap_or_else(|| vec![0.85, 0.2, 0.2]);
            let g = |i: usize| color.get(i).copied().unwrap_or(0.0) as f32;
            let size = spec.get("size").map(numbers).transpose()?.unwrap_or_else(|| vec![comp.width as f64, comp.height as f64]);
            let dim = |i: usize| limits::size(size.get(i).copied().unwrap_or(1.0).max(1.0) as u64);
            let pixel_aspect = spec.get("pixelAspect").and_then(J::as_f64).filter(|x| x.is_finite() && *x > 0.0).map_or(1.0, limits::pixel_aspect);
            let folder = proj.folder_named("Solids").unwrap_or_else(|| proj.add_item("Solids", Label::Yellow, None, ItemKind::Folder));
            let item = proj.add_item(
                name,
                Label::Red,
                Some(folder),
                ItemKind::Solid(Solid { color: [g(0), g(1), g(2)], width: dim(0), height: dim(1), pixel_aspect }),
            );
            LayerSource::Solid { item }
        }
        "footage" | "model" => {
            let path = spec.get("file").and_then(J::as_str).ok_or_else(|| format!("layer `{name}`: missing `file`"))?;
            let mut fj = to_json(&Footage::default());
            if let Some(d) = spec.get("footage") {
                merge_json(&mut fj, d);
            }
            merge_json(&mut fj, &json!({"path": path}));
            let f: Footage = serde_json::from_value(fj).map_err(|e| format!("layer `{name}`: bad `footage`: {e}"))?;
            let existing = proj.items.values().find(|i| matches!(&i.kind, ItemKind::Footage(x) if *x == f)).map(|i| i.id);
            let item = existing.unwrap_or_else(|| {
                let file = std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path).to_string();
                proj.add_item(&file, Label::Aqua, None, ItemKind::Footage(f))
            });
            if ty == "model" { LayerSource::Model { item } } else { LayerSource::Footage { item } }
        }
        "precomp" => {
            let key = spec.get("comp").and_then(J::as_str).ok_or_else(|| format!("layer `{name}`: missing `comp` (a name from `precomps`)"))?;
            LayerSource::Comp { item: *precomps.get(key).ok_or_else(|| format!("layer `{name}`: no precomp `{key}` in the recipe"))? }
        }
        t => return Err(format!("layer `{name}`: unknown type `{t}` (shape, solid, text, null, camera, light, primitive, footage, model, precomp)")),
    })
}

/// Build comp recipe `rc` (`{comp, layers}`) as a new item; returns it.
fn build_comp(proj: &mut Project, rc: &Map<String, J>, precomps: &HashMap<String, ItemId>, name: Option<&str>) -> R<ItemId> {
    let cj = rc.get("comp").and_then(J::as_object).ok_or("missing `comp` {name, width, height, frameRate, duration}")?;
    let mut c = comp_from_json(cj)?;
    let specs = match rc.get("layers") {
        None => vec![],
        Some(J::Array(a)) => a.iter().map(|l| l.as_object().ok_or_else(|| "each layer must be an object".to_string())).collect::<R<Vec<_>>>()?,
        Some(_) => return Err("`layers` must be an array".into()),
    };
    if specs.len() > MAX_LAYERS {
        return Err("too many layers".into());
    }
    let mut layers = vec![];
    for spec in &specs {
        let source = make_source(proj, spec, &c, precomps)?;
        let lname = spec.get("name").and_then(J::as_str).unwrap_or("Layer");
        layers.push(assemble(proj, &c, source, spec).map_err(|e| format!("layer `{lname}`: {e}"))?);
    }
    let dec = Dec { layers: layers.iter().map(|l| (l.name.clone(), l.id)).collect(), fr: c.frame_rate };
    for (l, spec) in layers.iter_mut().zip(&specs) {
        let lname = l.name.clone();
        apply_values(l, spec, &dec).map_err(|e| format!("layer `{lname}`: {e}"))?;
        apply_meta(l, spec, &dec).map_err(|e| format!("layer `{lname}`: {e}"))?;
    }
    c.layers = layers.into();
    let base = name.or_else(|| cj.get("name").and_then(J::as_str)).unwrap_or("Comp 1");
    let unique = proj.unique_item_name(base, |_| false);
    Ok(proj.add_item(&unique, Label::Sandstone, None, ItemKind::Comp(Arc::new(c))))
}

/// Build `recipe` (precomps first) in `proj`: (the comp, the precomps).
pub fn build_recipe(proj: &mut Project, recipe: &J, name: Option<&str>) -> R<(ItemId, Vec<ItemId>)> {
    let r = recipe.as_object().ok_or("a recipe is a JSON object {comp, layers, precomps?}")?;
    if let Some(f) = r.get("format").and_then(J::as_str)
        && f != FORMAT
    {
        return Err(format!("not a recipe (format `{f}`, expected `{FORMAT}`)"));
    }
    if let Some(v) = r.get("version").and_then(J::as_u64)
        && v > VERSION
    {
        return Err(format!("recipe version {v} is newer than this EffectCraft reads ({VERSION})"));
    }
    let mut keys = HashMap::new();
    let mut made = vec![];
    let pcs = r.get("precomps").and_then(J::as_array).map(Vec::as_slice).unwrap_or_default();
    if pcs.len() > MAX_PRECOMPS {
        return Err("too many precomps".into());
    }
    for pc in pcs {
        let pc = pc.as_object().ok_or("each precomp is a recipe {comp, layers}")?;
        let key = pc.get("comp").and_then(|c| c.get("name")).and_then(J::as_str).ok_or("each precomp's comp needs a `name`")?.to_string();
        let id = build_comp(proj, pc, &keys, None).map_err(|e| format!("precomp `{key}`: {e}"))?;
        keys.insert(key, id);
        made.push(id);
    }
    let id = build_comp(proj, r, &keys, name)?;
    Ok((id, made))
}

// ---------------------------------------------------------------- commands

fn recipe_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let only = match p.get("layers") {
        None | Some(J::Null) => None,
        Some(J::Array(a)) => Some(a.iter().map(|v| resolve_layer(comp, v).ok_or_else(|| bad(CMD_RECIPE, super::no_layer(v)))).collect::<Result<Vec<_>>>()?),
        Some(_) => return Err(bad(CMD_RECIPE, "`layers` must be an array of layer references")),
    };
    let all = b_p(p, "includeDefaults").unwrap_or(false);
    let r = recipe(&s.project, cid, only.as_deref(), all).map_err(|e| bad(CMD_RECIPE, e))?;
    if let Some(path) = str_p(p, "path").filter(|x| !x.is_empty()) {
        let text = serde_json::to_string_pretty(&r).map_err(|e| bad(CMD_RECIPE, e.to_string()))?;
        s.services.write_file(path, text.as_bytes()).map_err(|e| bad(CMD_RECIPE, format!("cannot write {path}: {e}")))?;
        let bytes = r.to_string().len();
        return Ok(
            json!({"path": path, "bytes": bytes, "layers": r["layers"].as_array().map_or(0, Vec::len), "unsupported": r.get("unsupported").cloned().unwrap_or(json!([]))}),
        );
    }
    Ok(r)
}

type Value = J;

/// Whether `v` nests deeper than `max` arrays and objects.
fn too_deep(v: &J, max: usize) -> bool {
    match v {
        _ if max == 0 => matches!(v, J::Array(_) | J::Object(_)),
        J::Array(a) => a.iter().any(|x| too_deep(x, max - 1)),
        J::Object(o) => o.values().any(|x| too_deep(x, max - 1)),
        _ => false,
    }
}

fn from_recipe_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let recipe = match (p.get("recipe"), str_p(p, "path")) {
        (Some(J::String(text)), _) => serde_json::from_str::<J>(text).map_err(|e| bad(CMD_BUILD, format!("`recipe` isn't JSON: {e}")))?,
        (Some(r @ J::Object(_)), _) => r.clone(),
        (Some(_), _) => return Err(bad(CMD_BUILD, "`recipe` must be the recipe object (or its JSON text)")),
        (None, Some(path)) => {
            let bytes = s.services.read_file(path).map_err(|e| bad(CMD_BUILD, format!("cannot read {path}: {e}")))?;
            serde_json::from_slice::<J>(&bytes).map_err(|e| bad(CMD_BUILD, format!("{path} isn't JSON: {e}")))?
        }
        (None, None) => return Err(bad(CMD_BUILD, "give `recipe` (from comp.recipe) or `path` (a recipe file)")),
    };
    if too_deep(&recipe, 200) {
        return Err(bad(CMD_BUILD, "the recipe is nested too deep"));
    }
    let name = str_p(p, "name").map(str::to_string);
    let (id, made) = s.edit("Composition from Recipe", None, |proj, st| {
        let r = build_recipe(proj, &recipe, name.as_deref()).map_err(|e| bad(CMD_BUILD, e))?;
        st.project_selection = vec![r.0];
        Ok(r)
    })?;
    if b_p(p, "open") != Some(false) {
        s.open_comp(id);
    }
    let comp = s.project.comp(id).ok_or(EngineError::NoComp)?;
    let name = s.project.item(id).map(|i| i.name.clone()).unwrap_or_default();
    Ok(json!({"comp": id.0, "name": name, "layers": comp.layers.len(), "precomps": made.iter().map(|i| i.0).collect::<Vec<_>>()}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        crate::query!(
            "comp.recipe",
            "Composition Recipe",
            "{comp?, layers?: [id|name|#n] (default every layer), path? (write the recipe there as JSON; then → {path, bytes, layers, unsupported}), includeDefaults? (also list properties at their defaults)} → {format: \"effectcraft-recipe\", version, comp: {name, width, height, frameRate, duration, background?, workArea?…}, layers: [{name, type: shape|solid|text|null|camera|light|primitive|footage|model|precomp, source (color/size, file, comp, light…), contents?/effects?/masks?/styles?/animators? (structure, in order), text? (Source Text changes), props?: {path: value | {keys: [{time, value, in?, out?}], expr?}}, groups?: {path: {name?, enabled?}}, start?/in?/out? (s), parent?, trackMatte?, blendMode?, switches?…}], precomps?: [recipes of nested comps], unsupported?: [{comp, layer?, what, why}]}: only what was authored, compared with freshly made layers of the same kind (docs/agents.md, Reuse a library comp)",
            recipe_cmd
        ),
        crate::cmd!(
            "comp.fromRecipe",
            "New Composition from Recipe",
            [],
            None,
            "{recipe? (a comp.recipe result, as an object or JSON text) | path? (a recipe file), name? (default the recipe's comp name, made unique), open? (default true)} → {comp, name, layers, precomps}: builds a new composition (and its precomps) from a recipe in one undo step; property paths, structure and values as comp.recipe writes them",
            super::always,
            from_recipe_cmd
        ),
    ]
}
