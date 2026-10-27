//! `layer.bounds`: where layers are on screen at a comp time (MCP `get_layout`). Headless agents
//! can't eyeball a render; this gives them each layer's comp-space box, visibility and overlaps.
//! The geometry itself is [`crate::layout`]. Also holds small helpers the other analysis queries
//! share.

use effectcraft_geom::vec2;
use effectcraft_project::{Comp, Layer, LayerId};
use effectcraft_render::{EvalCtx, RenderOpts, Renderer};
use serde_json::{Value, json};

use super::{CommandSpec, b_p, bad, comp_id, f_p, resolve_layer, time_p};
use crate::{EngineError, Result, Session, layout, query};

pub(super) fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
pub(super) fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
pub(super) fn r4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}
pub(super) fn pt(p: [f64; 2]) -> Value {
    json!([r3(p[0]), r3(p[1])])
}
pub(super) fn rect(r: [f64; 4]) -> Value {
    json!(r.map(r3))
}

/// The layers a query names (`layers: [..]` or `layer`), or every layer of the comp.
pub(super) fn named_layers(comp: &Comp, p: &Value, cmd: &str) -> Result<Vec<LayerId>> {
    let one = |v: &Value| resolve_layer(comp, v).ok_or_else(|| bad(cmd, format!("no layer {v} (use a layer id, \"#n\" or a name from comp.info)")));
    match p.get("layers").or_else(|| p.get("layer")).filter(|v| !v.is_null()) {
        Some(Value::Array(a)) => a.iter().map(one).collect(),
        Some(v) => Ok(vec![one(v)?]),
        None => Ok(comp.layers.iter().map(|l| l.id).collect()),
    }
}

/// A renderer with the session's footage, expressions, layer cache and preferences.
pub(super) fn renderer(s: &Session, opts: RenderOpts) -> Renderer<'_> {
    let opts = RenderOpts { nested_switches: s.prefs.general.switches_affect_nested_comps, draft_shadows: s.prefs.three_d.realtime_shadows, ..opts };
    let mut r = Renderer::new(&s.project, s.footage.as_ref(), opts);
    r.expr = s.expr.as_deref();
    r.cache = Some(&s.layer_cache);
    r
}

// ---------------------------------------------------------------- layer.bounds

/// Most layers whose pairwise overlaps are computed.
const MAX_OVERLAP_LAYERS: usize = 256;

fn layer_bounds(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.bounds";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let t = time_p(s, p, Some(comp));
    let ids = named_layers(comp, p, C)?;
    let pixels = b_p(p, "pixels").unwrap_or(false);
    let r = renderer(s, RenderOpts::default());
    let ctx = r.eval_ctx(cid, t).ok_or(EngineError::NoComp)?;
    let (w, h) = (comp.width as f64, comp.height as f64);
    let mut out = vec![];
    let mut quads: Vec<(LayerId, [[f64; 2]; 4], f64)> = vec![];
    for id in ids {
        let Some(l) = comp.layer(id) else { continue };
        let index = comp.layers.iter().position(|x| x.id == id).map_or(0, |i| i + 1);
        let (visible, why) = layout::visibility(&ctx, l);
        let tr = l.transform();
        let mut o = json!({
            "id": id.0,
            "index": index,
            "name": l.name,
            "type": l.source.type_name(),
            "visible": visible,
            "why": why,
            "opacity": r2(ctx.opacity(l) * 100.0),
            "threeD": l.is_3d(),
            "adjustment": l.switches.adjustment,
            "parent": l.parent.map(|p| p.0),
            "inPoint": l.in_point.seconds(),
            "outPoint": l.out_point.seconds(),
            "position": tr.map(|tr| { let v = ctx.position(l, tr); if l.is_3d() { json!(v.map(r3)) } else { json!([r3(v[0]), r3(v[1])]) } }),
            "anchor": tr.map(|tr| { let a = ctx.v3(l, tr, "anchor", [0.0; 3]); json!([r3(a[0]), r3(a[1])]) }),
            "anchorInComp": layout::anchor_in_comp(&ctx, l).map(pt),
        });
        let mattes: Vec<u64> = comp.layers.iter().filter(|x| x.track_matte.as_ref().is_some_and(|m| m.layer == id)).map(|x| x.id.0).collect();
        if !mattes.is_empty() {
            o["matteFor"] = json!(mattes);
        }
        match layout::layer_geometry(&ctx, l) {
            Some(g) => {
                o["source"] = json!({"left": r3(g.source[0]), "top": r3(g.source[1]), "width": r3(g.source[2]), "height": r3(g.source[3])});
                o["projected"] = json!(g.projected);
                if g.projected {
                    o["bounds"] = rect(g.bounds);
                    o["corners"] = json!(g.corners.map(pt));
                    o["center"] = pt(g.center);
                    o["onScreen"] = json!(r4(layout::on_screen_fraction(&g.corners, w, h)));
                    let area = layout::polygon_area(&g.corners);
                    if visible && area > 1e-9 {
                        quads.push((id, g.corners, area));
                    }
                } else {
                    o["bounds"] = Value::Null;
                    o["note"] = json!("part of this 3D layer is behind the active camera, so its corners can't be projected");
                }
            }
            None => {
                o["bounds"] = Value::Null;
                o["note"] = json!(if l.is_camera() || l.is_light() { "cameras and lights have no content bounds" } else { "no content at this time" });
            }
        }
        if pixels {
            o["pixelBounds"] = if visible { layout::pixel_bounds(&r, &ctx, l).map_or(Value::Null, rect) } else { Value::Null };
        }
        out.push(o);
    }
    let mut v = json!({"comp": cid.0, "time": t.seconds(), "width": comp.width, "height": comp.height, "layers": out});
    if b_p(p, "overlaps").unwrap_or(false) {
        let mut pairs = vec![];
        let q = quads.get(..MAX_OVERLAP_LAYERS.min(quads.len())).unwrap_or(&[]);
        for (i, a) in q.iter().enumerate() {
            for b in q.iter().skip(i + 1) {
                let area = layout::overlap_area(&a.1, &b.1);
                if area > 1e-6 {
                    pairs.push(json!({"layers": [a.0.0, b.0.0], "area": r2(area), "fraction": [r4(area / a.2), r4(area / b.2)]}));
                }
            }
        }
        v["overlaps"] = json!(pairs);
        if quads.len() > MAX_OVERLAP_LAYERS {
            v["overlapsNote"] = json!(format!("overlaps computed for the first {MAX_OVERLAP_LAYERS} visible layers"));
        }
    }
    Ok(v)
}

// ---------------------------------------------------------------- layer.at

/// Most layers a `layer.at` answer lists under the point.
const MAX_AT_HITS: usize = 32;
/// Below this alpha a pixel counts as empty (as `layout::pixel_bounds` reads it).
const EMPTY_ALPHA: f32 = 1.0 / 512.0;

/// The layer's drawn alpha at comp point `p` (masks, effects and styles included), or None when
/// its pixels can't be read (no buffer, a matrix that doesn't invert).
fn alpha_at(r: &Renderer, ctx: &EvalCtx, l: &Layer, p: [f64; 2]) -> Option<f32> {
    let buf = r.layer_buf(ctx, l)?;
    let (m, _) = ctx.layer_to_comp(l);
    let q = m.inverse()?.apply(vec2(p[0], p[1]));
    let (bx, by) = buf.to_px([q.x, q.y]);
    if !bx.is_finite() || !by.is_finite() {
        return None;
    }
    let (w, h) = (i64::from(buf.img.width), i64::from(buf.img.height));
    let (ix, iy) = (bx.floor() as i64, by.floor() as i64);
    if ix < 0 || iy < 0 || ix >= w || iy >= h {
        return Some(0.0);
    }
    let i = usize::try_from(iy.saturating_mul(w).saturating_add(ix)).ok()?;
    buf.img.data.get(i).map(|px| px[3])
}

/// Whether comp point `p` is inside the convex quad `q` (corners in order, either winding).
fn point_in_quad(p: [f64; 2], q: &[[f64; 2]; 4]) -> bool {
    let mut sign = 0.0;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        let c = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
        if c.abs() < 1e-9 {
            continue;
        }
        if sign == 0.0 {
            sign = c.signum();
        } else if c.signum() != sign {
            return false;
        }
    }
    true
}

/// Most precomp levels `layer.at {deep: true}` goes down (a cycle can't loop it).
const MAX_AT_DEPTH: usize = 8;

/// One comp's answer under a point: the topmost drawn layer (json and the layer), every layer
/// whose box covers the point, front to back.
fn hits_at<'a>(
    r: &Renderer<'a>,
    comp: &'a Comp,
    ctx: &EvalCtx,
    t: effectcraft_time::Tick,
    x: f64,
    y: f64,
    pixels: bool,
) -> (Value, Option<&'a Layer>, Vec<Value>) {
    let any_solo = comp.layers.iter().any(|l| l.switches.solo && l.source.is_av());
    let mut hits = vec![];
    let mut top = Value::Null;
    let mut top_layer = None;
    for (i, l) in comp.layers.iter().enumerate() {
        let candidate = l.is_active_at(t)
            && l.switches.video
            && !l.switches.adjustment
            && !l.switches.guide
            && !l.is_camera()
            && !l.is_light()
            && (!any_solo || l.switches.solo);
        if !candidate {
            continue;
        }
        let Some(g) = layout::layer_geometry(ctx, l).filter(|g| g.projected) else { continue };
        if !point_in_quad([x, y], &g.corners) {
            continue;
        }
        let alpha = if pixels { alpha_at(r, ctx, l, [x, y]) } else { None };
        let drawn = alpha.is_none_or(|a| a > EMPTY_ALPHA);
        let o = json!({
            "id": l.id.0,
            "index": i + 1,
            "name": l.name,
            "type": l.source.type_name(),
            "locked": l.switches.locked,
            "alpha": alpha.map(|a| r3(f64::from(a))),
            "drawn": drawn,
        });
        if top.is_null() && drawn {
            top = o.clone();
            top_layer = Some(l);
        }
        hits.push(o);
        if hits.len() >= MAX_AT_HITS {
            break;
        }
    }
    (top, top_layer, hits)
}

/// The layers under a comp point, front to back, and the topmost one that draws a pixel there.
/// Unlike the viewer's click it counts locked layers, which an agent
/// pointing at the picture still means, and skips adjustment and guide layers, which cover the
/// frame without being anything one points at. Read-only.
///
/// `deep: true` also follows a precomp hit down: the point goes through the precomp layer's
/// transform into its comp's pixels, the time through its time remap / stretch, and the nested
/// comp is asked again, up to [`MAX_AT_DEPTH`] levels. `path` lists every level, outermost
/// first; its last `layer` is the deepest layer drawn there (Ghostex's chips name it: a click on
/// a card inside a scene precomp means the card, 27 Oct).
fn layer_at(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.at";
    let cid = comp_id(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let t = time_p(s, p, Some(comp));
    let (Some(x), Some(y)) = (f_p(p, "x"), f_p(p, "y")) else {
        return Err(bad(C, "x and y (comp pixels from the top left) are required"));
    };
    if !x.is_finite() || !y.is_finite() {
        return Err(bad(C, "x and y must be finite numbers"));
    }
    let pixels = b_p(p, "pixels").unwrap_or(true);
    let deep = b_p(p, "deep").unwrap_or(false);
    let r = renderer(s, RenderOpts::default());
    let ctx = r.eval_ctx(cid, t).ok_or(EngineError::NoComp)?;
    let comp_name = |id| s.project.item(id).map(|i| i.name.as_str()).unwrap_or("");
    let (top, mut top_layer, hits) = hits_at(&r, comp, &ctx, t, x, y, pixels);
    let mut v = json!({"comp": cid.0, "compName": comp_name(cid), "time": t.seconds(), "point": pt([x, y]), "layer": top, "hits": hits});
    if !deep {
        return Ok(v);
    }
    let mut path = vec![json!({"comp": cid.0, "compName": comp_name(cid), "time": t.seconds(), "point": pt([x, y]), "layer": v["layer"].clone()})];
    let mut level = ctx;
    let mut at = (x, y);
    while let Some(l) = top_layer.take() {
        if path.len() > MAX_AT_DEPTH {
            // Eight levels down already (or a cycle): the deepest so far is the answer.
            break;
        }
        let effectcraft_project::LayerSource::Comp { item } = l.source else { break };
        let Some(nested) = s.project.comp(item) else { break };
        let (m, _) = level.layer_to_comp(l);
        let Some(inv) = m.inverse() else { break };
        let q = inv.apply(vec2(at.0, at.1));
        if !q.x.is_finite() || !q.y.is_finite() {
            break;
        }
        let nt = level.nested_time(l);
        let Some(nctx) = r.eval_ctx(item, nt) else { break };
        let (ntop, nlayer, _) = hits_at(&r, nested, &nctx, nt, q.x, q.y, pixels);
        if ntop.is_null() {
            break;
        }
        path.push(json!({"comp": item.0, "compName": comp_name(item), "time": nt.seconds(), "point": pt([q.x, q.y]), "layer": ntop}));
        top_layer = nlayer;
        level = nctx;
        at = (q.x, q.y);
    }
    v["path"] = json!(path);
    Ok(v)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "layer.at",
            "Layer At Point",
            "{comp?, time?|frame?, x, y (comp pixels), pixels? (default true: the layer must draw a pixel there, not only cover it with its box), deep? (default false: also follow a precomp hit into its comp, up to 8 levels)} → {comp, compName, time, point, layer: the topmost layer drawn at the point {id, index, name, type, locked, alpha, drawn} or null, hits: every layer whose box covers it, front to back, path (deep only): [{comp, compName, time, point, layer}] outermost first, the last layer the deepest one drawn there}. Counts locked layers; skips adjustment, guide, camera and light layers",
            layer_at
        ),
        query!(
            "layer.bounds",
            "Layer Bounds",
            "{comp?, time?|frame?, layers?: [id|name|#n] (default every layer), pixels? (also measure the drawn pixels: masks, effects, styles), overlaps? (pairwise overlaps of the visible layers)} → per layer: visible + why, comp-space bounds [x,y,w,h], corners, center, anchorInComp, onScreen fraction, source rect",
            layer_bounds
        ),
    ]
}
