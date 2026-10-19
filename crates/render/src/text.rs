//! Text layers: Source Text layout → text animators (Range / Wiggly / Expression selectors,
//! combined by mode) → per-character transforms and paint → outlines, optionally placed on a mask
//! path (Path Options) and drawn with More Options (anchor point grouping, fill & stroke order,
//! inter-character blending). Per-character 3D characters are handed to the 3D compositor as
//! separate planes (see [`per_char_planes`]).

use std::borrow::Cow;

use effectcraft_color::BlendMode;
use effectcraft_effects::Buf;
use effectcraft_geom::{Mat3, Mat4, Vec3, vec2, vec3};
use effectcraft_keyframe::{Gradient, Justify, TextDoc, Value};
use effectcraft_path::{BezPath, FillRule, StrokeStyle};
use effectcraft_project::{Layer, PropGroup};
use effectcraft_raster::Image;
use effectcraft_text::path_text::{self, PathMeasure};
use effectcraft_text::selectors::{self, BasedOn, Mode, Range, Shape, Wiggly};
use effectcraft_text::{CharGlyph, TextLayout, char_glyph_style, layout_doc};

use crate::eval::EvalCtx;

/// Evaluated per-character animation state (sums of every animator, weighted by selection).
#[derive(Clone, Copy, Debug)]
pub struct CharXf {
    pub offset: [f64; 3],
    pub anchor: [f64; 3],
    pub scale: [f64; 3],
    /// Z rotation (2D rotation).
    pub rotation: f64,
    pub rotation_x: f64,
    pub rotation_y: f64,
    pub skew: f64,
    pub skew_axis: f64,
    pub opacity: f64,
    pub fill: Option<[f32; 4]>,
    pub fill_k: f32,
    /// Hue (degrees), saturation and brightness (percent) offsets.
    pub fill_hsb: [f64; 3],
    pub fill_opacity: f64,
    pub stroke: Option<[f32; 4]>,
    pub stroke_k: f32,
    pub stroke_hsb: [f64; 3],
    pub stroke_opacity: f64,
    pub stroke_width: f64,
    /// Tracking (1/1000 em) added before / after the character.
    pub track_before: f64,
    pub track_after: f64,
    /// Line Anchor (percent) and the selection weight it was set with.
    pub line_anchor: f64,
    pub line_anchor_k: f64,
    pub line_spacing: [f64; 2],
    pub char_offset: f64,
    /// Character Value (code point) and its selection.
    pub char_value: Option<(f64, f64)>,
    pub char_range: u32,
    pub char_align: u32,
    pub blur: [f64; 2],
    /// Variable Font Axes offsets (axis tag, user units), the first `n_axes` used.
    pub axes: [([u8; 4], f64); 4],
    pub n_axes: u8,
    /// Front / Bevel / Side / Back Color of the extruded character (Advanced 3D), by
    /// `effectcraft_model::extrude::Surface` index, and the selection each was set with.
    pub surface: [Option<[f32; 4]>; 4],
    pub surface_k: [f32; 4],
}

impl CharXf {
    /// The surface colours of an extrusion painted `base` (the character's fill or stroke):
    /// each animated surface colour blended over `base` by its selection.
    pub fn surface_colors(&self, base: [f32; 4]) -> crate::shapes::SurfaceColors {
        std::array::from_fn(|i| {
            let (c, k) = (self.surface.get(i).copied().flatten()?, self.surface_k.get(i).copied().unwrap_or(0.0));
            Some(std::array::from_fn(|d| base[d] + (c[d] - base[d]) * k))
        })
    }
    /// The variable-font axis offsets that move the outline.
    pub fn axis_deltas(&self) -> Vec<(String, f32)> {
        self.axes[..self.n_axes as usize]
            .iter()
            .filter(|(_, d)| d.abs() > 1e-6)
            .map(|(t, d)| (String::from_utf8_lossy(t).trim_end().to_string(), *d as f32))
            .collect()
    }
    fn add_axis(&mut self, tag: [u8; 4], d: f64) {
        if let Some(a) = self.axes[..self.n_axes as usize].iter_mut().find(|a| a.0 == tag) {
            a.1 += d;
        } else if (self.n_axes as usize) < self.axes.len() {
            self.axes[self.n_axes as usize] = (tag, d);
            self.n_axes += 1;
        }
    }
}

/// Animator property match ids of Front / Bevel / Side / Back Color, in
/// `effectcraft_model::extrude::Surface::ALL` order.
pub const SURFACE_ANIM_PROPS: [&str; 4] = ["frontColor", "bevelColor", "sideColor", "backColor"];

/// Animator property match id of a Variable Font Axes property (`axis_wght`).
pub const AXIS_PREFIX: &str = "axis_";

impl Default for CharXf {
    fn default() -> Self {
        CharXf {
            offset: [0.0; 3],
            anchor: [0.0; 3],
            scale: [100.0; 3],
            rotation: 0.0,
            rotation_x: 0.0,
            rotation_y: 0.0,
            skew: 0.0,
            skew_axis: 0.0,
            opacity: 100.0,
            fill: None,
            fill_k: 0.0,
            fill_hsb: [0.0; 3],
            fill_opacity: 100.0,
            stroke: None,
            stroke_k: 0.0,
            stroke_hsb: [0.0; 3],
            stroke_opacity: 100.0,
            stroke_width: 0.0,
            track_before: 0.0,
            track_after: 0.0,
            line_anchor: 0.0,
            line_anchor_k: 0.0,
            line_spacing: [0.0; 2],
            char_offset: 0.0,
            char_value: None,
            char_range: 0,
            char_align: 1,
            blur: [0.0; 2],
            axes: [([0; 4], 0.0); 4],
            n_axes: 0,
            surface: [None; 4],
            surface_k: [0.0; 4],
        }
    }
}

/// The unit a glyph belongs to for a selector's Based On, and the unit count.
fn unit_of(based: BasedOn, g: &CharGlyph, lay: &TextLayout) -> (usize, usize) {
    match based {
        BasedOn::Characters => (g.char_index, lay.chars),
        BasedOn::CharactersExcludingSpaces => (g.char_index_no_space.min(lay.chars_no_space.saturating_sub(1)), lay.chars_no_space),
        BasedOn::Words => (g.word_index, lay.words),
        BasedOn::Lines => (g.line_index, lay.lines),
    }
}

fn unit_count(based: BasedOn, lay: &TextLayout) -> usize {
    match based {
        BasedOn::Characters => lay.chars,
        BasedOn::CharactersExcludingSpaces => lay.chars_no_space,
        BasedOn::Words => lay.words,
        BasedOn::Lines => lay.lines,
    }
}

fn layer_seed(layer: &Layer, seed: f64) -> u64 {
    (seed as i64 as u64) ^ layer.id.0.wrapping_mul(0x9E37_79B9)
}

/// Range Selector parameters at the context time (fractions of `n` units).
fn range_params(ctx: &EvalCtx, layer: &Layer, sel: &PropGroup, n: usize) -> Range {
    let adv = sel.sub("advanced");
    let af = |m: &str, d: f64| adv.map(|a| ctx.f(layer, a, m, d)).unwrap_or(d);
    let units_index = adv.map(|a| ctx.e(layer, a, "units") == 1).unwrap_or(false);
    let div = if units_index { n.max(1) as f64 } else { 100.0 };
    Range {
        // Start and End stay within 0..100% (0..n in Index units); Offset within -100..100%.
        start: ctx.f(layer, sel, "start", 0.0).clamp(0.0, div) / div,
        end: ctx.f(layer, sel, "end", if units_index { n as f64 } else { 100.0 }).clamp(0.0, div) / div,
        offset: ctx.f(layer, sel, "offset", 0.0).clamp(-div, div) / div,
        amount: af("amount", 100.0) / 100.0,
        shape: Shape::from_index(adv.map(|a| ctx.e(layer, a, "shape")).unwrap_or(0)),
        smoothness: af("smoothness", 100.0) / 100.0,
        ease_high: af("easeHigh", 0.0) / 100.0,
        ease_low: af("easeLow", 0.0) / 100.0,
    }
}

/// Selection of every glyph by an animator's selectors, per dimension.
pub fn animator_selection(ctx: &EvalCtx, layer: &Layer, anim: &PropGroup, lay: &TextLayout) -> Vec<[f64; 3]> {
    let sels: Vec<&PropGroup> = anim.sub("selectors").map(|s| s.groups().filter(|g| g.enabled).collect()).unwrap_or_default();
    if sels.is_empty() {
        // No selectors: every character is fully affected.
        return vec![[1.0; 3]; lay.glyphs.len()];
    }
    let mut acc: Vec<[f64; 3]> = vec![[0.0; 3]; lay.glyphs.len()];
    let lt = layer.layer_time(ctx.time).seconds();
    for (si, sel) in sels.iter().enumerate() {
        let (mode, based) = match sel.match_id.as_str() {
            "rangeSelector" => {
                let adv = sel.sub("advanced");
                (adv.map(|a| ctx.e(layer, a, "mode")).unwrap_or(0), adv.map(|a| ctx.e(layer, a, "basedOn")).unwrap_or(0))
            }
            "wigglySelector" => (ctx.e(layer, sel, "mode"), ctx.e(layer, sel, "basedOn")),
            _ => (0, ctx.e(layer, sel, "basedOn")),
        };
        let mode = Mode::from_index(mode);
        let based = BasedOn::from_index(based);
        let n = unit_count(based, lay);
        let values: Box<dyn Fn(usize, usize, [f64; 3]) -> [f64; 3]> = match sel.match_id.as_str() {
            "rangeSelector" => {
                let r = range_params(ctx, layer, sel, n);
                let adv = sel.sub("advanced");
                let perm =
                    adv.filter(|a| ctx.b(layer, a, "randomize")).map(|a| selectors::random_order(n, layer_seed(layer, ctx.f(layer, a, "randomSeed", 0.0))));
                Box::new(move |i, n, _| {
                    let i = perm.as_ref().and_then(|p| p.get(i).copied()).unwrap_or(i);
                    [selectors::range_value(&r, i, n); 3]
                })
            }
            "wigglySelector" => {
                let w = Wiggly {
                    max: ctx.f(layer, sel, "maxAmount", 100.0) / 100.0,
                    min: ctx.f(layer, sel, "minAmount", -100.0) / 100.0,
                    wiggles_per_second: ctx.f(layer, sel, "wigglesPerSecond", 2.0),
                    correlation: ctx.f(layer, sel, "correlation", 50.0) / 100.0,
                    temporal_phase: ctx.f(layer, sel, "temporalPhase", 0.0),
                    spatial_phase: ctx.f(layer, sel, "spatialPhase", 0.0),
                    lock_dimensions: ctx.b(layer, sel, "lockDimensions"),
                    seed: layer_seed(layer, ctx.f(layer, sel, "randomSeed", 0.0)),
                };
                Box::new(move |i, _, _| [0, 1, 2].map(|d| selectors::wiggly_value(&w, lt, i, d)))
            }
            "expressionSelector" => {
                let Some(prop) = sel.get("amount") else { continue };
                let stat = ctx.v3(layer, sel, "amount", [100.0; 3]);
                Box::new(move |i, n, prev| {
                    let pv = prev.map(|x| x * 100.0);
                    // Without an expression engine: the static amount scales the selection.
                    let fallback = [0, 1, 2].map(|d| stat[d] * prev[d]);
                    let r = match ctx.expr.filter(|_| prop.has_expression()) {
                        Some(h) => h.eval_text_selector(ctx, layer, prop, i, n, pv).unwrap_or(fallback),
                        None => fallback,
                    };
                    r.map(|x| (x / 100.0).clamp(-1.0, 1.0))
                })
            }
            _ => continue,
        };
        // The Expression Selector's result replaces the selection (it reads it as selectorValue).
        let replace = sel.match_id == "expressionSelector";
        let mut memo: std::collections::HashMap<(usize, [u64; 3]), [f64; 3]> = std::collections::HashMap::new();
        for (gi, g) in lay.glyphs.iter().enumerate() {
            let (i, n) = unit_of(based, g, lay);
            let prev = match si {
                0 if replace => [1.0; 3],
                0 => [mode.initial(); 3],
                _ => acc[gi],
            };
            let key = (i, prev.map(f64::to_bits));
            let v = *memo.entry(key).or_insert_with(|| values(i, n, prev));
            acc[gi] = if replace { v } else { [0, 1, 2].map(|d| mode.combine(prev[d], v[d])) };
        }
    }
    acc
}

/// Blend an animator's colour `f` at selection `k` over the colour the animators above it left,
/// as After Effects does: each animator lerps from the colour so far. `(acc, acc_k)` stays one
/// lerp from the base colour (`lerp(base, acc, acc_k)`), composed exactly:
/// `K' = 1 − (1−K)(1−k)`, `F' = (F·K·(1−k) + f·k) / K'`. The first animator sets `(f, k)` as is.
fn blend_over(acc: &mut Option<[f32; 4]>, acc_k: &mut f32, f: [f32; 4], k: f64) {
    let k = (k as f32).clamp(0.0, 1.0);
    match *acc {
        Some(prev) if *acc_k > 0.0 => {
            if k <= 0.0 {
                return;
            }
            let kk = 1.0 - (1.0 - *acc_k) * (1.0 - k);
            let w = *acc_k * (1.0 - k);
            *acc = Some(if kk > 0.0 { std::array::from_fn(|d| (prev[d] * w + f[d] * k) / kk) } else { f });
            *acc_k = kk;
        }
        _ => {
            *acc = Some(f);
            *acc_k = k;
        }
    }
}

/// A Gradient Fill animator's gradient at the context time (an EffectCraft extension, see
/// `effectcraft_project::build::text_gradient_fill_props`).
#[derive(Clone, Debug, PartialEq)]
pub struct GradSpec {
    pub gradient: Gradient,
    /// Start and end in percent of the group's box (0,0 top left, 100,100 bottom right).
    pub start: [f64; 2],
    pub end: [f64; 2],
    pub radial: bool,
}

/// One colour animator of a glyph's fill, in animator order, while the fill has a gradient: a
/// flat Fill Color or a Gradient Fill of character group `group` (an index into the layer's
/// [`GradGroup`]s), each at its selection.
#[derive(Clone, Debug)]
enum RawFill {
    Flat([f32; 4], f32),
    Grad { spec: std::sync::Arc<GradSpec>, k: f32, group: usize },
}

/// One layer of a glyph's per-pixel fill (see [`PlacedGlyph::fills`]): a flat colour, or a
/// gradient whose start and end are points `s`, `e` of the unanimated layout (layer space).
#[derive(Clone, Debug)]
pub enum GlyphFill {
    Flat([f32; 4], f32),
    Grad { spec: std::sync::Arc<GradSpec>, k: f32, s: [f64; 2], e: [f64; 2] },
}

impl GlyphFill {
    /// The gradient's colour at point `p` of the unanimated layout.
    fn color_at(spec: &GradSpec, s: [f64; 2], e: [f64; 2], p: [f64; 2]) -> [f32; 4] {
        let (dx, dy) = (e[0] - s[0], e[1] - s[1]);
        let len2 = dx * dx + dy * dy;
        let t = if !len2.is_finite() || len2 <= 1e-12 {
            0.0
        } else if spec.radial {
            ((p[0] - s[0]).powi(2) + (p[1] - s[1]).powi(2)).sqrt() / len2.sqrt()
        } else {
            ((p[0] - s[0]) * dx + (p[1] - s[1]) * dy) / len2
        };
        spec.gradient.sample(if t.is_finite() { t } else { 0.0 })
    }
}

/// A glyph's fill while a Gradient Fill selects it: the character's own fill colour and the
/// colour animators over it, in order.
#[derive(Clone, Debug)]
pub struct GradientFill {
    pub base: [f32; 4],
    pub layers: Vec<GlyphFill>,
}

/// The fill of a glyph with Gradient Fill layers at point `p` of the unanimated layout: the
/// base with each layer lerped over it by its selection in order, then Hue / Saturation /
/// Brightness / Fill Opacity as for a flat fill.
pub fn gradient_fill_at(fill: &GradientFill, xf: &CharXf, p: [f64; 2]) -> [f32; 4] {
    let mut c = fill.base;
    for f in &fill.layers {
        let (col, k) = match f {
            GlyphFill::Flat(col, k) => (*col, *k),
            GlyphFill::Grad { spec, k, s, e } => (GlyphFill::color_at(spec, *s, *e, p), *k),
        };
        let k = k.clamp(0.0, 1.0);
        c = std::array::from_fn(|d| c[d] + (col[d] - c[d]) * k);
    }
    adjust_color(c, None, 0.0, xf.fill_hsb, xf.fill_opacity)
}

/// A group of characters one Gradient Fill animator paints with one gradient (Based On Words:
/// a word; Lines: a line; Characters: a run of consecutive selected characters; no selector:
/// the whole text).
#[derive(Clone, Debug, PartialEq)]
pub struct GradGroup {
    /// The animator's uid.
    pub animator: u64,
    /// The characters' outlines in the unanimated layout (layer space): x0, y0, x1, y1.
    pub bbox: [f64; 4],
    /// Gradient Start / End there (layer space of the unanimated layout).
    pub start: [f64; 2],
    pub end: [f64; 2],
    /// The character (index into the text) whose animation carries the handles: the middle one.
    pub char_index: usize,
    /// Every character of the group (indices into the text).
    pub chars: Vec<usize>,
}

impl GradGroup {
    /// A point of the box in percent (0,0 top left, 100,100 bottom right) → layer space of the
    /// unanimated layout.
    pub fn from_percent(bbox: [f64; 4], p: [f64; 2]) -> [f64; 2] {
        [bbox[0] + (bbox[2] - bbox[0]) * p[0] / 100.0, bbox[1] + (bbox[3] - bbox[1]) * p[1] / 100.0]
    }
    /// The inverse of [`GradGroup::from_percent`] (a zero-size side maps to 0).
    pub fn to_percent(bbox: [f64; 4], p: [f64; 2]) -> [f64; 2] {
        let f = |v: f64, a: f64, b: f64| if (b - a).abs() > 1e-9 { (v - a) / (b - a) * 100.0 } else { 0.0 };
        [f(p[0], bbox[0], bbox[2]), f(p[1], bbox[1], bbox[3])]
    }
}

/// Animator state per glyph, and while any animator has a Gradient Fill, every glyph's fill
/// layers and the character groups (char indices) the gradients are drawn across.
struct Animated {
    xfs: Vec<CharXf>,
    fills: Option<Vec<Vec<RawFill>>>,
    groups: Vec<(u64, Vec<usize>)>,
}

/// The Based On of an animator's first selector (`None` without selectors).
fn first_based_on(ctx: &EvalCtx, layer: &Layer, anim: &PropGroup) -> Option<BasedOn> {
    let sel = anim.sub("selectors")?.groups().find(|g| g.enabled)?;
    let i = match sel.match_id.as_str() {
        "rangeSelector" => sel.sub("advanced").map(|a| ctx.e(layer, a, "basedOn")).unwrap_or(0),
        _ => ctx.e(layer, sel, "basedOn"),
    };
    Some(BasedOn::from_index(i))
}

/// Which group (index into `groups`) each glyph belongs to for a Gradient Fill animator, by the
/// first selector's Based On; `None` for glyphs it does not select.
fn gradient_groups(based: Option<BasedOn>, sel: &[[f64; 3]], lay: &TextLayout, animator: u64, groups: &mut Vec<(u64, Vec<usize>)>) -> Vec<Option<usize>> {
    let mut out = vec![None; lay.glyphs.len()];
    let mut by_unit: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut run: Option<(usize, usize)> = None;
    for (gi, g) in lay.glyphs.iter().enumerate() {
        let k = sel.get(gi).map_or(0.0, |k| k[0]);
        // Spaces draw nothing: a selected one keeps a run of characters going.
        if g.is_space {
            if k <= 0.0 {
                run = None;
            }
            continue;
        }
        let unit = match based {
            None => Some(0),
            Some(BasedOn::Words) => Some(g.word_index),
            Some(BasedOn::Lines) => Some(g.line_index),
            Some(BasedOn::Characters | BasedOn::CharactersExcludingSpaces) => None,
        };
        let id = match unit {
            Some(u) => {
                if k <= 0.0 {
                    continue;
                }
                *by_unit.entry(u).or_insert_with(|| {
                    groups.push((animator, vec![]));
                    groups.len() - 1
                })
            }
            None => {
                if k <= 0.0 {
                    run = None;
                    continue;
                }
                match run {
                    Some((line, id)) if line == g.line_index => id,
                    _ => {
                        groups.push((animator, vec![]));
                        let id = groups.len() - 1;
                        run = Some((g.line_index, id));
                        id
                    }
                }
            }
        };
        if let Some(gr) = groups.get_mut(id) {
            gr.1.push(g.char_index);
        }
        out[gi] = Some(id);
    }
    out
}

/// Evaluate animators for every glyph.
pub fn char_transforms(ctx: &EvalCtx, layer: &Layer, text: &PropGroup, lay: &TextLayout) -> Vec<CharXf> {
    animate(ctx, layer, text, lay).xfs
}

fn animate(ctx: &EvalCtx, layer: &Layer, text: &PropGroup, lay: &TextLayout) -> Animated {
    let mut out = vec![CharXf::default(); lay.glyphs.len()];
    let Some(anims) = text.sub("animators") else { return Animated { xfs: out, fills: None, groups: vec![] } };
    let with_gradient = anims.groups().any(|a| a.enabled && a.sub("properties").is_some_and(|p| p.get("gradientFillColors").is_some()));
    let mut fills: Option<Vec<Vec<RawFill>>> = with_gradient.then(|| vec![vec![]; lay.glyphs.len()]);
    let mut groups: Vec<(u64, Vec<usize>)> = vec![];
    for anim in anims.groups().filter(|g| g.enabled) {
        let Some(props) = anim.sub("properties") else { continue };
        let sel = animator_selection(ctx, layer, anim, lay);
        let grad = props.get("gradientFillColors").and_then(|p| match ctx.value(layer, p) {
            Value::Gradient(g) => Some(*g),
            _ => None,
        });
        let grad = grad.map(|gradient| {
            let pct = |m: &str, d: [f64; 2]| props.get(m).map(|p| ctx.value(layer, p).as_vec2()).unwrap_or(d);
            let radial = props.get("gradientType").is_some_and(|p| ctx.value(layer, p).as_enum() == 1);
            let spec = GradSpec { gradient, start: pct("gradientStart", [0.0, 50.0]), end: pct("gradientEnd", [100.0, 50.0]), radial };
            (std::sync::Arc::new(spec), gradient_groups(first_based_on(ctx, layer, anim), &sel, lay, anim.uid, &mut groups))
        });
        let get = |m: &str| props.get(m).map(|p| ctx.value(layer, p).components());
        let get1 = |m: &str| props.get(m).map(|p| ctx.value(layer, p).as_f64());
        let c3 = |v: Vec<f64>, d: f64| [v.first().copied().unwrap_or(d), v.get(1).copied().unwrap_or(d), v.get(2).copied().unwrap_or(d)];
        let pos = get("position").map(|v| c3(v, 0.0));
        let anchor = get("anchor").map(|v| c3(v, 0.0));
        let scale = get("scale").map(|v| c3(v, 100.0));
        let rot = get1("rotation");
        let rx = get1("rotationX");
        let ry = get1("rotationY");
        let skew = get1("skew");
        let skew_axis = get1("skewAxis");
        let op = get1("opacity");
        let fill = props.get("fillColor").map(|p| ctx.value(layer, p).as_color());
        let stroke = props.get("strokeColor").map(|p| ctx.value(layer, p).as_color());
        let fill_hsb = [get1("fillHue"), get1("fillSaturation"), get1("fillBrightness")];
        let stroke_hsb = [get1("strokeHue"), get1("strokeSaturation"), get1("strokeBrightness")];
        let fill_op = get1("fillOpacity");
        let stroke_op = get1("strokeOpacity");
        let sw = get1("strokeWidth");
        let tracking = get1("tracking");
        let tracking_type = props.get("trackingType").map(|p| ctx.value(layer, p).as_enum()).unwrap_or(0);
        let line_anchor = get1("lineAnchor");
        let line_spacing = props.get("lineSpacing").map(|p| ctx.value(layer, p).as_vec2());
        let char_offset = get1("characterOffset");
        let char_value = get1("characterValue");
        let char_range = props.get("characterRange").map(|p| ctx.value(layer, p).as_enum());
        let char_align = props.get("characterAlignment").map(|p| ctx.value(layer, p).as_enum());
        let blur = props.get("blur").map(|p| ctx.value(layer, p).as_vec2());
        let surface = SURFACE_ANIM_PROPS.map(|m| props.get(m).map(|p| ctx.value(layer, p).as_color()));
        let axes: Vec<([u8; 4], f64)> = props
            .props()
            .filter_map(|p| {
                let t = p.match_id.strip_prefix(AXIS_PREFIX)?;
                let mut tag = [b' '; 4];
                for (i, c) in t.bytes().take(4).enumerate() {
                    tag[i] = c;
                }
                Some((tag, ctx.value(layer, p).as_f64()))
            })
            .collect();
        for (gi, k3) in sel.iter().enumerate() {
            let k = k3[0];
            if k3.iter().all(|x| *x == 0.0) {
                continue;
            }
            let c = &mut out[gi];
            for d in 0..3 {
                if let Some(p) = pos {
                    c.offset[d] += p[d] * k3[d];
                }
                if let Some(a) = anchor {
                    c.anchor[d] += a[d] * k3[d];
                }
                if let Some(s) = scale {
                    c.scale[d] *= 1.0 + (s[d] / 100.0 - 1.0) * k3[d];
                }
            }
            if let Some(r) = rot {
                c.rotation += r * k;
            }
            if let Some(r) = rx {
                c.rotation_x += r * k;
            }
            if let Some(r) = ry {
                c.rotation_y += r * k;
            }
            if let Some(s) = skew {
                c.skew += s * k;
            }
            if let Some(a) = skew_axis {
                c.skew_axis = a;
            }
            if let Some(o) = op {
                c.opacity *= 1.0 + (o / 100.0 - 1.0) * k;
            }
            if let Some(f) = fill {
                blend_over(&mut c.fill, &mut c.fill_k, f, k);
                if let Some(layers) = fills.as_mut().and_then(|f| f.get_mut(gi)) {
                    layers.push(RawFill::Flat(f, k as f32));
                }
            }
            if let Some((spec, by_glyph)) = &grad
                && let Some(Some(group)) = by_glyph.get(gi)
                && let Some(layers) = fills.as_mut().and_then(|f| f.get_mut(gi))
            {
                layers.push(RawFill::Grad { spec: spec.clone(), k: k as f32, group: *group });
            }
            if let Some(f) = stroke {
                blend_over(&mut c.stroke, &mut c.stroke_k, f, k);
            }
            for d in 0..3 {
                if let Some(v) = fill_hsb[d] {
                    c.fill_hsb[d] += v * k;
                }
                if let Some(v) = stroke_hsb[d] {
                    c.stroke_hsb[d] += v * k;
                }
            }
            if let Some(o) = fill_op {
                c.fill_opacity *= 1.0 + (o / 100.0 - 1.0) * k;
            }
            if let Some(o) = stroke_op {
                c.stroke_opacity *= 1.0 + (o / 100.0 - 1.0) * k;
            }
            if let Some(w) = sw {
                c.stroke_width += w * k;
            }
            if let Some(t) = tracking {
                let t = t * k;
                match tracking_type {
                    1 => c.track_before += t,
                    2 => c.track_after += t,
                    _ => {
                        c.track_before += t * 0.5;
                        c.track_after += t * 0.5;
                    }
                }
            }
            if let Some(a) = line_anchor
                && k.abs() >= c.line_anchor_k
            {
                c.line_anchor = a;
                c.line_anchor_k = k.abs();
            }
            if let Some(l) = line_spacing {
                c.line_spacing[0] += l[0] * k;
                c.line_spacing[1] += l[1] * k;
            }
            if let Some(o) = char_offset {
                c.char_offset += o * k;
            }
            if let Some(v) = char_value {
                c.char_value = Some((v, k));
            }
            if let Some(r) = char_range {
                c.char_range = r;
            }
            if let Some(a) = char_align {
                c.char_align = a;
            }
            if let Some(b) = blur {
                c.blur[0] += (b[0] * k3[0]).abs();
                c.blur[1] += (b[1] * k3[1]).abs();
            }
            for (tag, v) in &axes {
                c.add_axis(*tag, v * k);
            }
            for ((s, sk), v) in c.surface.iter_mut().zip(&mut c.surface_k).zip(surface) {
                if let Some(v) = v {
                    blend_over(s, sk, v, k);
                }
            }
        }
    }
    // A glyph no gradient selects keeps the flat fill (the solid path, as before).
    if let Some(f) = fills.as_mut() {
        for layers in f.iter_mut() {
            if !layers.iter().any(|l| matches!(l, RawFill::Grad { k, .. } if *k > 0.0)) {
                layers.clear();
            }
        }
    }
    Animated { xfs: out, fills, groups }
}

/// Character Offset / Character Value substitution (None = unchanged).
pub fn substitute(ch: char, x: &CharXf) -> Option<char> {
    if ch.is_whitespace() {
        return None;
    }
    let mut code = ch as u32 as f64;
    if let Some((v, k)) = x.char_value {
        code += (v - code) * k;
    }
    let off = x.char_offset.round() as i64;
    let base = code.round().max(0.0) as u32;
    let out = if x.char_range == 1 {
        char::from_u32((base as i64 + off).max(0) as u32)
    } else {
        let c = char::from_u32(base)?;
        let wrap = |lo: char, n: i64| char::from_u32((lo as i64 + ((c as i64 - lo as i64 + off).rem_euclid(n))) as u32);
        if c.is_ascii_lowercase() {
            wrap('a', 26)
        } else if c.is_ascii_uppercase() {
            wrap('A', 26)
        } else if c.is_ascii_digit() {
            wrap('0', 10)
        } else {
            Some(c)
        }
    }?;
    (out != ch).then_some(out)
}

/// The Source Text value at the current time.
pub fn source_text(ctx: &EvalCtx, layer: &Layer) -> Option<TextDoc> {
    let text = layer.props.sub("text")?;
    match ctx.group_value(layer, text, "sourceText")? {
        Value::Text(t) => Some(*t),
        Value::Str(s) => Some(TextDoc { text: s, ..Default::default() }),
        _ => None,
    }
}

/// Per-character 3D is on for this layer (and the layer is 3D).
pub fn per_char_3d(ctx: &EvalCtx, layer: &Layer) -> bool {
    layer.is_3d() && layer.props.sub("text").is_some_and(|t| ctx.b(layer, t, "perChar3d"))
}

/// One drawn character.
#[derive(Clone, Debug)]
pub struct PlacedGlyph {
    /// Outline in character space (the anchor pivot at the origin).
    pub local: BezPath,
    /// Character space → layer space.
    pub m: Mat4,
    pub xf: CharXf,
    pub fill: [f32; 4],
    pub stroke: [f32; 4],
    pub stroke_width: f64,
    /// The character's style fills it (Character panel fill on).
    pub apply_fill: bool,
    /// Index of the character in the text.
    pub char_index: usize,
    /// Where the pivot sits in the unanimated layout (layer space): `m · (p − rest)` carries a
    /// point of the static layout to where the animation puts this character.
    pub rest: [f64; 2],
    /// A Gradient Fill selects this character: the fill is painted per pixel from the base fill
    /// and these layers ([`gradient_fill_at`] at the point of the unanimated layout), and
    /// [`PlacedGlyph::fill`] holds its colour at the middle of the first gradient (for lint,
    /// extrusions and text → shapes). `None`: the flat `fill`, as without gradients.
    pub fills: Option<std::sync::Arc<GradientFill>>,
}

impl PlacedGlyph {
    /// Character space → layer space, flattened to 2D (ignores Z).
    pub fn m2(&self) -> Mat3 {
        let m = &self.m.0;
        Mat3([[m[0][0], m[0][1], m[0][3]], [m[1][0], m[1][1], m[1][3]], [0.0, 0.0, 1.0]])
    }
    /// Outline in layer space (2D).
    pub fn path(&self) -> BezPath {
        effectcraft_path::transform(std::slice::from_ref(&self.local), &self.m2()).remove(0)
    }
}

/// Laid-out, animated text of a layer at the context time.
#[derive(Clone)]
pub struct TextGeom {
    pub doc: TextDoc,
    pub glyphs: Vec<PlacedGlyph>,
    /// Fill & Stroke: 0 per character palette, 1 all fills over all strokes, 2 all strokes over
    /// all fills.
    pub fill_stroke: u32,
    pub blend: BlendMode,
    /// The Gradient Fill character groups, by animator in order, each in text order.
    pub grad_groups: Vec<GradGroup>,
}

fn adjust_color(base: [f32; 4], rgb: Option<[f32; 4]>, k: f32, hsb: [f64; 3], opacity: f64) -> [f32; 4] {
    let mut c = match rgb {
        Some(f) => [base[0] + (f[0] - base[0]) * k, base[1] + (f[1] - base[1]) * k, base[2] + (f[2] - base[2]) * k, base[3] + (f[3] - base[3]) * k],
        None => base,
    };
    if hsb != [0.0; 3] {
        let (h, s, v) = effectcraft_color::rgb_to_hsv(c[0], c[1], c[2]);
        let h = h + (hsb[0] / 360.0) as f32;
        let s = (s + (hsb[1] / 100.0) as f32).clamp(0.0, 1.0);
        let v = (v + (hsb[2] / 100.0) as f32).clamp(0.0, 1.0);
        let (r, g, b) = effectcraft_color::hsv_to_rgb(h, s, v);
        c = [r, g, b, c[3]];
    }
    c[3] *= (opacity / 100.0).clamp(0.0, 1.0) as f32;
    c
}

/// The mask path chosen in Path Options (layer space), measured.
fn text_path(ctx: &EvalCtx, layer: &Layer, text: &PropGroup) -> Option<(PathMeasure, bool, bool, f64, f64)> {
    let po = text.sub("pathOptions")?;
    let k = ctx.e(layer, po, "path") as usize;
    if k == 0 {
        return None;
    }
    let mask = layer.masks()?.groups().nth(k - 1)?;
    let Some(Value::Path(sp)) = ctx.group_value(layer, mask, "path") else { return None };
    let bp = effectcraft_path::to_kurbo(&sp);
    let pm = PathMeasure::new(&bp, ctx.b(layer, po, "reversePath"));
    if pm.is_empty() {
        return None;
    }
    let perpendicular = po.get("perpendicular").map(|p| ctx.value(layer, p).as_bool()).unwrap_or(true);
    Some((pm, perpendicular, ctx.b(layer, po, "forceAlignment"), ctx.f(layer, po, "firstMargin", 0.0), ctx.f(layer, po, "lastMargin", 0.0)))
}

/// For every caret position `0..=chars` of the layer's text, the map from the static layout
/// (layer space, what [`TextLayout::caret`] returns) to where animators and Path Options put
/// the characters: the caret follows the character before it (or the first one).
pub fn caret_maps(ctx: &EvalCtx, layer: &Layer, chars: usize) -> Vec<Mat3> {
    let Some(geom) = text_geom(ctx, layer) else { return vec![Mat3::IDENTITY; chars + 1] };
    let mut by_char: Vec<Option<Mat3>> = vec![None; chars + 1];
    for g in &geom.glyphs {
        if let Some(slot) = by_char.get_mut(g.char_index) {
            *slot = Some(g.m2() * Mat3::translate(effectcraft_geom::vec2(-g.rest[0], -g.rest[1])));
        }
    }
    let first = by_char.iter().flatten().next().copied().unwrap_or(Mat3::IDENTITY);
    let mut out = Vec::with_capacity(chars + 1);
    let mut last = first;
    for ci in 0..=chars {
        // Caret ci sits after character ci − 1.
        if let Some(Some(m)) = ci.checked_sub(1).map(|p| by_char[p]) {
            last = m;
        }
        out.push(if ci == 0 { by_char[0].unwrap_or(first) } else { last });
    }
    out
}

/// A paragraph's alignment: 0 left, 1 centre, 2 right (a justified paragraph by its last line).
fn align_index(j: Justify) -> u8 {
    match j {
        Justify::Center | Justify::JustifyLastCenter => 1,
        Justify::Right | Justify::JustifyLastRight => 2,
        _ => 0,
    }
}

/// Text geometry remembered while one frame renders (EXPR-PERF6). A text layer is laid out and
/// animated by `text::render`, and again by `raster_scale` (measuring, several calls per layer
/// and frame when the layer is scaled up or the viewer zoomed in) and by culling; each time ran
/// every animator selector's expressions per unit. Entries are pure functions of the frame's
/// project (immutably borrowed by the renderer for the whole scope, so its address names its
/// contents), the comp time, the expression host and footage, and the font set; a scope's
/// entries are dropped when it closes. Only layers whose text has expressions are remembered
/// (for the others a copy would cost about as much as laying them out again).
#[derive(Default)]
struct GeomMemo {
    /// The project the innermost open scope renders (address).
    active: Option<usize>,
    maps: std::collections::HashMap<usize, std::collections::HashMap<GeomKey, Option<TextGeom>>>,
}

/// (comp id, layer id, comp time, expression host address, footage address, font revision).
type GeomKey = (u64, u64, i64, usize, usize, u64);

/// Text layers remembered per scope at most.
const MAX_GEOMS: usize = 256;

thread_local! {
    static GEOMS: std::cell::RefCell<GeomMemo> = std::cell::RefCell::new(GeomMemo::default());
}

/// While alive, [`text_geom`] remembers geometry for the project it was opened with, on this
/// thread (see [`GeomMemo`]). Opened by the renderer around a frame; nested opens for the same
/// project do nothing.
pub(crate) struct GeomScope {
    /// What was active before (restored on close) and whether this scope opened `addr`.
    prev: Option<usize>,
    addr: usize,
    owns: bool,
}

impl GeomScope {
    pub(crate) fn open(project: &effectcraft_project::Project) -> GeomScope {
        let addr = std::ptr::from_ref(project) as usize;
        GEOMS.with(|g| {
            let mut g = g.borrow_mut();
            let prev = g.active;
            let owns = prev != Some(addr);
            if owns {
                g.maps.remove(&addr);
                g.active = Some(addr);
            }
            GeomScope { prev, addr, owns }
        })
    }
}

impl Drop for GeomScope {
    fn drop(&mut self) {
        if self.owns {
            GEOMS.with(|g| {
                let mut g = g.borrow_mut();
                g.maps.remove(&self.addr);
                g.active = self.prev;
            });
        }
    }
}

/// Whether any property under `g` has an expression.
fn any_expression(g: &PropGroup) -> bool {
    g.children.iter().any(|c| match c {
        effectcraft_project::Node::Prop(p) => p.has_expression(),
        effectcraft_project::Node::Group(sub) => any_expression(sub),
    })
}

/// Lay out and animate a text layer.
pub fn text_geom(ctx: &EvalCtx, layer: &Layer) -> Option<TextGeom> {
    let addr = std::ptr::from_ref(ctx.project) as usize;
    let own = ctx.project.comp(ctx.comp_id).is_some_and(|c| std::ptr::eq(c, ctx.comp)) && ctx.comp.layer(layer.id).is_some_and(|l| std::ptr::eq(l, layer));
    let usable = own && GEOMS.with(|g| g.borrow().active == Some(addr)) && layer.props.sub("text").is_some_and(any_expression);
    if !usable {
        return text_geom_uncached(ctx, layer);
    }
    let expr = ctx.expr.map_or(0, |e| std::ptr::from_ref(e).cast::<()>() as usize);
    let footage = ctx.footage.map_or(0, |f| std::ptr::from_ref(f).cast::<()>() as usize);
    let key: GeomKey = (ctx.comp_id.0, layer.id.0, ctx.time.0, expr, footage, effectcraft_text::fonts::revision());
    if let Some(hit) = GEOMS.with(|g| g.borrow().maps.get(&addr).and_then(|m| m.get(&key)).cloned()) {
        return hit;
    }
    // Computed outside the borrow: expressions may lay out other text layers (sourceRectAtTime).
    let geom = text_geom_uncached(ctx, layer);
    GEOMS.with(|g| {
        let mut g = g.borrow_mut();
        let m = g.maps.entry(addr).or_default();
        if m.len() >= MAX_GEOMS {
            m.clear();
        }
        m.insert(key, geom.clone());
    });
    geom
}

fn text_geom_uncached(ctx: &EvalCtx, layer: &Layer) -> Option<TextGeom> {
    let doc = source_text(ctx, layer)?;
    let text = layer.props.sub("text")?;
    let lay = layout_doc(&doc);
    let Animated { xfs, fills: raw_fills, groups } = animate(ctx, layer, text, &lay);
    let three = per_char_3d(ctx, layer);
    let more = text.sub("moreOptions");
    let grouping = more.map(|m| ctx.e(layer, m, "anchorGrouping")).unwrap_or(0);
    let galign = more.map(|m| ctx.v2(layer, m, "groupingAlignment", [0.0; 2])).unwrap_or([0.0; 2]);
    let fill_stroke = more.map(|m| ctx.e(layer, m, "fillStroke")).unwrap_or(0);
    let blend_idx = more.map(|m| ctx.e(layer, m, "interCharBlend")).unwrap_or(0) as usize;
    let blend = effectcraft_project::build::INTER_CHAR_BLEND_MODES.get(blend_idx).and_then(|n| BlendMode::from_name(n)).unwrap_or_default();

    // Character substitutions (Character Offset / Value). Adjust Kerning re-lays out the text.
    let mut subs: Vec<Option<char>> = lay.glyphs.iter().zip(&xfs).map(|(g, x)| substitute(g.ch, x)).collect();
    let relayout = subs.iter().zip(&xfs).any(|(s, x)| s.is_some() && x.char_align == 3);
    let (lay, xfs, raw_fills) = if relayout {
        let mut chars: Vec<char> = doc.text.chars().collect();
        for (g, s) in lay.glyphs.iter().zip(&subs) {
            if let (Some(c), Some(slot)) = (s, chars.get_mut(g.char_index)) {
                *slot = *c;
            }
        }
        // Same character count: the style runs still line up.
        let d2 = TextDoc { text: chars.into_iter().collect(), ..doc.clone() };
        let l2 = layout_doc(&d2);
        let by_char: std::collections::HashMap<usize, CharXf> = lay.glyphs.iter().zip(&xfs).map(|(g, x)| (g.char_index, *x)).collect();
        let x2: Vec<CharXf> = l2.glyphs.iter().map(|g| by_char.get(&g.char_index).copied().unwrap_or_default()).collect();
        let f2 = raw_fills.map(|f| {
            let by_char: std::collections::HashMap<usize, &Vec<RawFill>> = lay.glyphs.iter().zip(&f).map(|(g, x)| (g.char_index, x)).collect();
            l2.glyphs.iter().map(|g| by_char.get(&g.char_index).map(|v| (*v).clone()).unwrap_or_default()).collect::<Vec<_>>()
        });
        subs = vec![None; l2.glyphs.len()];
        (l2, x2, f2)
    } else {
        (lay, xfs, raw_fills)
    };

    // Variable Font Axes: the advance at the animated design-space position (later characters
    // on the line move with it).
    let n = lay.glyphs.len();
    let vadv: Vec<f64> = (0..n)
        .map(|gi| match (subs[gi], xfs[gi].axis_deltas()) {
            (None, d) if !d.is_empty() => effectcraft_text::variable::char_advance_delta(&lay.glyphs[gi], &d).unwrap_or(0.0),
            _ => 0.0,
        })
        .collect();
    let advance = |gi: usize| lay.glyphs[gi].advance + vadv[gi];
    // Tracking (before / after each character) and Line Anchor, per line. Without a Line Anchor
    // a line grows from where its paragraph's alignment pins it (left, centre or right).
    let paras = doc.paras();
    let mut shift = vec![0.0f64; n];
    let mut li_start = 0;
    while li_start < n {
        let line = lay.glyphs[li_start].line_index;
        let mut li_end = li_start;
        while li_end < n && lay.glyphs[li_end].line_index == line {
            li_end += 1;
        }
        let mut pen = 0.0;
        let justify = lay.layout.lines.get(line).and_then(|l| paras.get(l.para)).map_or(doc.justify, |p| p.justify);
        let (mut la, mut la_k) = (50.0 * f64::from(align_index(justify)), 0.0);
        for gi in li_start..li_end {
            let x = &xfs[gi];
            let em = lay.styles.get(lay.glyphs[gi].run).map_or(doc.size, |s| s.size) / 1000.0;
            pen += x.track_before * em;
            shift[gi] = pen;
            pen += x.track_after * em + vadv[gi];
            if x.line_anchor_k > la_k {
                la = x.line_anchor;
                la_k = x.line_anchor_k;
            }
        }
        let back = pen * la / 100.0;
        for s in &mut shift[li_start..li_end] {
            *s -= back;
        }
        li_start = li_end;
    }

    // Glyph origins after tracking / line spacing, and pivots by anchor point grouping.
    let origin = |gi: usize| -> [f64; 2] {
        let g = &lay.glyphs[gi];
        let x = &xfs[gi];
        [g.origin.x + shift[gi] + x.line_spacing[0] * g.line_index as f64, g.origin.y + x.line_spacing[1] * g.line_index as f64]
    };
    let group_key = |g: &CharGlyph| -> usize {
        match grouping {
            1 => g.word_index * 4096 + g.line_index,
            2 => g.line_index,
            3 => 0,
            _ => usize::MAX,
        }
    };
    let mut extents: std::collections::HashMap<usize, [f64; 4]> = std::collections::HashMap::new();
    if grouping != 0 {
        for gi in 0..n {
            let g = &lay.glyphs[gi];
            if g.is_space && grouping == 1 {
                continue;
            }
            let o = origin(gi);
            let e = extents.entry(group_key(g)).or_insert([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY]);
            e[0] = e[0].min(o[0]);
            e[2] = e[2].max(o[0] + advance(gi));
            e[1] = e[1].min(o[1]);
            e[3] = e[3].max(o[1]);
        }
    }

    // Path Options.
    let on_path = text_path(ctx, layer, text);
    let align = align_index(doc.justify);
    let first_baseline = lay.line_boxes.first().map(|b| b[1]).unwrap_or(0.0);
    let mut arc = vec![0.0f64; n];
    if let Some((pm, _, force, fm, lm)) = &on_path {
        let mut i = 0;
        while i < n {
            let line = lay.glyphs[i].line_index;
            let mut j = i;
            while j < n && lay.glyphs[j].line_index == line {
                j += 1;
            }
            let xs: Vec<f64> = (i..j).map(|gi| origin(gi)[0] + advance(gi) / 2.0).collect();
            let lb = lay.line_boxes.get(line).copied().unwrap_or([0.0; 4]);
            let pos = path_text::arc_positions(&xs, [lb[0], lb[0] + lb[2]], align, pm.length(), *fm, *lm, *force);
            arc[i..j].copy_from_slice(&pos);
            i = j;
        }
    }

    let base = doc.base_style();
    let mut glyphs = Vec::with_capacity(n);
    // Gradient Fill: each group's box in the unanimated layout, and the glyphs to finish.
    let mut boxes: Vec<[f64; 4]> = vec![[f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY]; groups.len()];
    let mut pending: Vec<(usize, Vec<RawFill>, [f32; 4])> = vec![];
    for gi in 0..n {
        let g = &lay.glyphs[gi];
        let mut x = xfs[gi];
        if g.is_space {
            continue;
        }
        if !three {
            x.offset[2] = 0.0;
            x.anchor[2] = 0.0;
            x.scale[2] = 100.0;
            x.rotation_x = 0.0;
            x.rotation_y = 0.0;
        }
        let o = origin(gi);
        let st = lay.styles.get(g.run).unwrap_or(&base);
        let size = st.size;
        // Outline (substituted characters aligned in the original slot).
        let (outline, adv) = match subs[gi] {
            Some(c) => {
                let (p, a) = char_glyph_style(st, c);
                let dx = match x.char_align {
                    0 => 0.0,
                    2 => g.advance - a,
                    _ => (g.advance - a) / 2.0,
                };
                (kurbo::Affine::translate((dx, 0.0)) * p, g.advance)
            }
            // Variable Font Axes: the outline redrawn at the animated design-space position.
            None => match x.axis_deltas() {
                d if !d.is_empty() => (effectcraft_text::variable::char_outline_varied(g, &d).unwrap_or_else(|| g.path.clone()), advance(gi)),
                _ => (g.path.clone(), g.advance),
            },
        };
        if outline.elements().is_empty() {
            continue;
        }
        let pivot = match extents.get(&group_key(g)) {
            Some(e) if grouping != 0 => [(e[0] + e[2]) / 2.0 + galign[0] / 100.0 * (e[2] - e[0]), (e[1] + e[3]) / 2.0 + galign[1] / 100.0 * size],
            _ => [o[0] + adv / 2.0 + galign[0] / 100.0 * adv, o[1] + galign[1] / 100.0 * size],
        };
        // Outline relative to the pivot.
        let local = kurbo::Affine::translate((o[0] - pivot[0], o[1] - pivot[1])) * outline;
        let char_m = Mat4::rotate_z(x.rotation)
            * Mat4::rotate_y(x.rotation_y)
            * Mat4::rotate_x(x.rotation_x)
            * Mat4::from_mat3_affine(&Mat3::skew_deg(-x.skew, x.skew_axis))
            * Mat4::scale(Vec3::from(x.scale) / 100.0)
            * Mat4::translate(-Vec3::from(x.anchor));
        let m = match &on_path {
            Some((pm, perp, ..)) => {
                // Position X slides along the path, Y moves off it.
                let char_center = o[0] + adv / 2.0;
                let s = arc[gi] + (pivot[0] - char_center) + x.offset[0];
                let (p, ang) = path_text::place(pm, s, pivot[1] - first_baseline + x.offset[1], *perp);
                Mat4::translate(vec3(p.x, p.y, x.offset[2])) * Mat4::rotate_z(ang) * char_m
            }
            None => Mat4::translate(vec3(pivot[0] + x.offset[0], pivot[1] + x.offset[1], x.offset[2])) * char_m,
        };
        let fill = adjust_color(st.fill, x.fill, x.fill_k, x.fill_hsb, x.fill_opacity);
        let stroke = adjust_color(st.stroke, x.stroke, x.stroke_k, x.stroke_hsb, x.stroke_opacity);
        let base_stroke_w = if st.apply_stroke { st.stroke_width } else { 0.0 };
        let rest = [pivot[0] - (o[0] - g.origin.x), pivot[1] - (o[1] - g.origin.y)];
        if let Some(layers) = raw_fills.as_ref().and_then(|f| f.get(gi)).filter(|l| !l.is_empty()) {
            // The outline in the unanimated layout: character space + rest.
            if let Some(b) = effectcraft_path::bounds(std::slice::from_ref(&local)) {
                let (x0, y0, x1, y1) = (b.x0 + rest[0], b.y0 + rest[1], b.x1 + rest[0], b.y1 + rest[1]);
                for l in layers {
                    if let RawFill::Grad { group, .. } = l
                        && let Some(bx) = boxes.get_mut(*group)
                    {
                        *bx = [bx[0].min(x0), bx[1].min(y0), bx[2].max(x1), bx[3].max(y1)];
                    }
                }
            }
            pending.push((glyphs.len(), layers.clone(), st.fill));
        }
        glyphs.push(PlacedGlyph {
            local,
            m,
            xf: x,
            fill,
            stroke,
            stroke_width: (base_stroke_w + x.stroke_width).max(0.0),
            apply_fill: st.apply_fill,
            char_index: g.char_index,
            rest,
            fills: None,
        });
    }
    let grad_groups = finish_gradients(&mut glyphs, pending, &groups, &boxes);
    Some(TextGeom { doc, glyphs, fill_stroke, blend, grad_groups })
}

/// Gradient Fill: each group's start and end points from its box, and each selected glyph's
/// fill layers with them (its flat `fill` becomes its colour at the middle of the gradient).
fn finish_gradients(
    glyphs: &mut [PlacedGlyph],
    pending: Vec<(usize, Vec<RawFill>, [f32; 4])>,
    groups: &[(u64, Vec<usize>)],
    boxes: &[[f64; 4]],
) -> Vec<GradGroup> {
    let mut specs: Vec<Option<std::sync::Arc<GradSpec>>> = vec![None; groups.len()];
    for (_, layers, _) in &pending {
        for l in layers {
            if let RawFill::Grad { spec, group, .. } = l
                && let Some(slot) = specs.get_mut(*group)
            {
                *slot = Some(spec.clone());
            }
        }
    }
    let ends: Vec<Option<([f64; 2], [f64; 2])>> = specs
        .iter()
        .zip(boxes)
        .map(|(spec, b)| {
            let spec = spec.as_ref()?;
            (b[0].is_finite() && b[2] >= b[0]).then(|| (GradGroup::from_percent(*b, spec.start), GradGroup::from_percent(*b, spec.end)))
        })
        .collect();
    for (gi, layers, st_fill) in pending {
        let Some(g) = glyphs.get_mut(gi) else { continue };
        let fills: Vec<GlyphFill> = layers
            .into_iter()
            .filter_map(|l| match l {
                RawFill::Flat(c, k) => Some(GlyphFill::Flat(c, k)),
                RawFill::Grad { spec, k, group } => {
                    let (s, e) = ends.get(group).copied().flatten()?;
                    Some(GlyphFill::Grad { spec, k, s, e })
                }
            })
            .collect();
        let mid = fills.iter().find_map(|f| match f {
            GlyphFill::Grad { s, e, .. } => Some([(s[0] + e[0]) / 2.0, (s[1] + e[1]) / 2.0]),
            GlyphFill::Flat(..) => None,
        });
        if let Some(mid) = mid {
            let gf = GradientFill { base: st_fill, layers: fills };
            g.fill = gradient_fill_at(&gf, &g.xf, mid);
            g.fills = Some(std::sync::Arc::new(gf));
        }
    }
    groups
        .iter()
        .zip(boxes)
        .zip(&ends)
        .filter_map(|(((animator, chars), b), se)| {
            let (start, end) = (*se)?;
            let drawn: Vec<usize> = chars.iter().copied().filter(|c| glyphs.iter().any(|g| g.char_index == *c)).collect();
            let char_index = drawn.get(drawn.len() / 2).or(chars.first()).copied()?;
            Some(GradGroup { animator: *animator, bbox: *b, start, end, char_index, chars: chars.clone() })
        })
        .collect()
}

/// Outlines of all glyphs in layer space with their animation state (for text → shapes, bounds
/// and hit testing).
pub fn glyph_paths(ctx: &EvalCtx, layer: &Layer) -> Vec<(BezPath, CharXf)> {
    text_geom(ctx, layer).map(|t| t.glyphs.iter().map(|g| (g.path(), g.xf)).collect()).unwrap_or_default()
}

/// One paint pass of a glyph: fill (false) or stroke (true).
type Pass = (usize, bool);

fn passes(geom: &TextGeom) -> Vec<Pass> {
    let doc = &geom.doc;
    let n = geom.glyphs.len();
    let fill_on = |i: usize| geom.glyphs[i].apply_fill;
    let has_stroke = |i: usize| geom.glyphs[i].stroke_width > 0.0;
    let mut v = Vec::with_capacity(n * 2);
    match geom.fill_stroke {
        1 => {
            v.extend((0..n).filter(|i| has_stroke(*i)).map(|i| (i, true)));
            v.extend((0..n).filter(|i| fill_on(*i)).map(|i| (i, false)));
        }
        2 => {
            v.extend((0..n).filter(|i| fill_on(*i)).map(|i| (i, false)));
            v.extend((0..n).filter(|i| has_stroke(*i)).map(|i| (i, true)));
        }
        _ => {
            for i in 0..n {
                let f = fill_on(i).then_some((i, false));
                let s = has_stroke(i).then_some((i, true));
                if doc.stroke_over_fill {
                    v.extend(f);
                    v.extend(s);
                } else {
                    v.extend(s);
                    v.extend(f);
                }
            }
        }
    }
    v
}

/// Rasterize one pass of a glyph — or of a run of glyphs painted alike, as one outline — (paths
/// mapped into target pixels by `m`) into a premultiplied patch over the target rect, blurred
/// when the character has Blur.
fn raster_pass(g: &PlacedGlyph, stroke: bool, paths: &[BezPath], m: &Mat3, rect: [i64; 4], blur_px: [f64; 2]) -> Image {
    let (w, h) = ((rect[2] - rect[0]) as u32, (rect[3] - rect[1]) as u32);
    let mm = Mat3::translate(vec2(-rect[0] as f64, -rect[1] as f64)) * *m;
    let cov = if stroke {
        let st = StrokeStyle { width: g.stroke_width, join: effectcraft_path::Join::Round, ..Default::default() };
        effectcraft_path::stroke_coverage(paths, &st, &mm, w, h)
    } else {
        effectcraft_path::fill_coverage(paths, &mm, w, h, FillRule::NonZero)
    };
    let op = (g.xf.opacity / 100.0).clamp(0.0, 1.0) as f32;
    // Gradient Fill: the colour per pixel, at the pixel centre's point of the unanimated layout
    // (one glyph per pass: `m` maps its character space, `rest` carries that to the layout).
    if !stroke
        && let Some(fills) = &g.fills
        && let Some(inv) = mm.inverse()
    {
        let colors: Vec<[f32; 4]> = (0..h as usize)
            .flat_map(|y| (0..w as usize).map(move |x| (x, y)))
            .zip(&cov.data)
            .map(|((x, y), c)| {
                if *c <= 0.0 {
                    return [0.0; 4];
                }
                let q = inv.apply(vec2(x as f64 + 0.5, y as f64 + 0.5));
                gradient_fill_at(fills, &g.xf, [q.x + g.rest[0], q.y + g.rest[1]])
            })
            .collect();
        return paint_coverage_colors(&cov.data, &colors, op, w, h, blur_px);
    }
    let col = if stroke { g.stroke } else { g.fill };
    paint_coverage(&cov.data, col, col[3] * op, w, h, blur_px)
}

/// [`paint_coverage`] with a colour per pixel (straight RGBA): premultiplied colour × coverage,
/// each channel blurred on its own when the character has Blur.
fn paint_coverage_colors(cov: &[f32], colors: &[[f32; 4]], op: f32, w: u32, h: u32, blur_px: [f64; 2]) -> Image {
    let mut img = Image::new(w, h);
    for ((px, c), col) in img.data.iter_mut().zip(cov).zip(colors) {
        let k = c * col[3] * op;
        if k > 0.0 {
            *px = [col[0] * k, col[1] * k, col[2] * k, k];
        }
    }
    if blur_px[0] <= 0.05 && blur_px[1] <= 0.05 {
        return img;
    }
    let (pw, ph) = (w as usize, h as usize);
    let planes: Vec<Vec<f32>> = (0..4)
        .map(|d| {
            let plane: Vec<f32> = img.data.iter().map(|p| p[d]).collect();
            effectcraft_raster::gaussian_blur_plane(&plane, pw, ph, blur_px[0] / 2.0, blur_px[1] / 2.0, false)
        })
        .collect();
    for (i, px) in img.data.iter_mut().enumerate() {
        *px = std::array::from_fn(|d| planes.get(d).and_then(|p| p.get(i)).copied().unwrap_or(0.0));
    }
    img
}

/// A `w`×`h` patch of colour `col` (alpha `a`) × `cov`, blurred by `blur_px` (premultiplied).
fn paint_coverage(cov: &[f32], col: [f32; 4], a: f32, w: u32, h: u32, blur_px: [f64; 2]) -> Image {
    let mut img = Image::new(w, h);
    // Before any blur: colour × coverage where covered.
    let k: Vec<f32> = cov.iter().map(|c| c * a).map(|k| if k > 0.0 { k } else { 0.0 }).collect();
    if blur_px[0] <= 0.05 && blur_px[1] <= 0.05 {
        for (px, &k) in img.data.iter_mut().zip(&k) {
            if k > 0.0 {
                *px = [col[0] * k, col[1] * k, col[2] * k, k];
            }
        }
        return img;
    }
    // Each channel is the blur of colour × coverage, one plane at a time: a channel of colour 1
    // is the coverage's own blur and one of colour 0 stays 0, so white, black and grey text blur
    // one or two planes instead of four channels (the same sums, so the same pixels).
    let (pw, ph) = (w as usize, h as usize);
    let blur = |p: &[f32]| effectcraft_raster::gaussian_blur_plane(p, pw, ph, blur_px[0] / 2.0, blur_px[1] / 2.0, false);
    let alpha = blur(&k);
    let mut planes: Vec<(u32, Vec<f32>)> = vec![];
    for c in col.iter().take(3) {
        if *c != 1.0 && c.to_bits() != 0 && !planes.iter().any(|(b, _)| *b == c.to_bits()) {
            let ck: Vec<f32> = k.iter().map(|&k| if k > 0.0 { c * k } else { 0.0 }).collect();
            planes.push((c.to_bits(), blur(&ck)));
        }
    }
    let chan = |c: f32| -> Option<&[f32]> { if c == 1.0 { Some(&alpha) } else { planes.iter().find(|(b, _)| *b == c.to_bits()).map(|(_, p)| p.as_slice()) } };
    let (r, g2, b) = (chan(col[0]), chan(col[1]), chan(col[2]));
    let at = |p: Option<&[f32]>, i: usize| p.and_then(|p| p.get(i).copied()).unwrap_or(0.0);
    for (i, px) in img.data.iter_mut().enumerate() {
        *px = [at(r, i), at(g2, i), at(b, i), alpha.get(i).copied().unwrap_or(0.0)];
    }
    img
}

fn blit(dst: &mut Image, src: &Image, at: [i64; 2], mode: BlendMode) {
    for y in 0..src.height as i64 {
        let ty = y + at[1];
        if ty < 0 || ty >= dst.height as i64 {
            continue;
        }
        for x in 0..src.width as i64 {
            let tx = x + at[0];
            if tx < 0 || tx >= dst.width as i64 {
                continue;
            }
            let s = src.data[(y * src.width as i64 + x) as usize];
            if s[3] <= 0.0 && s[0] == 0.0 && s[1] == 0.0 && s[2] == 0.0 {
                continue;
            }
            let d = &mut dst.data[(ty * dst.width as i64 + tx) as usize];
            *d = effectcraft_color::blend_pixel(mode, *d, s, 0.5);
        }
    }
}

/// An opaque, unblurred fill pass: where such fills of one colour touch or overlap, compositing
/// them one by one would let their anti-aliased edges show the background through.
fn solid_fill(g: &PlacedGlyph, stroke: bool) -> bool {
    !stroke && g.fills.is_none() && g.xf.blur == [0.0; 2] && g.fill[3] >= 1.0 && g.xf.opacity >= 100.0
}

/// The passes in drawing order, consecutive opaque fills of one colour gathered into a run that
/// is rasterised as one outline, as After Effects draws a line of text: glyphs that touch (the
/// crossbars of "ff") or overlap join without a seam (#415). Under an Inter-Character Blending
/// mode other than Normal every character is still composited onto the others on its own.
fn runs(geom: &TextGeom) -> Vec<(Vec<usize>, bool)> {
    let mut out: Vec<(Vec<usize>, bool)> = vec![];
    for (gi, stroke) in passes(geom) {
        let g = &geom.glyphs[gi];
        if g.xf.opacity <= 0.0 {
            continue;
        }
        if geom.blend == BlendMode::Normal
            && solid_fill(g, stroke)
            && let Some((run, false)) = out.last_mut()
            && run.last().is_some_and(|&p| solid_fill(&geom.glyphs[p], false) && geom.glyphs[p].fill == g.fill)
        {
            run.push(gi);
            continue;
        }
        out.push((vec![gi], stroke));
    }
    out
}

/// One pass (or run) of [`draw_glyphs`], ready to rasterise: the glyph it is painted like, its
/// outlines and their matrix into the target, the target rect it covers and its blur.
struct GlyphJob<'g> {
    g: &'g PlacedGlyph,
    stroke: bool,
    paths: Cow<'g, [BezPath]>,
    m: Mat3,
    rect: [i64; 4],
    blur: [f64; 2],
}

impl GlyphJob<'_> {
    fn raster(&self) -> Image {
        raster_pass(self.g, self.stroke, &self.paths, &self.m, self.rect, self.blur)
    }

    fn area(&self) -> u64 {
        ((self.rect[2] - self.rect[0]).max(0) as u64).saturating_mul((self.rect[3] - self.rect[1]).max(0) as u64)
    }
}

/// Below this many patch pixels a text layer is drawn on one thread.
const PARALLEL_PX: u64 = 1 << 16;
/// At most this many patch pixels are rasterised ahead of their blit (memory).
const BATCH_PX: u64 = 1 << 22;

/// How [`draw_glyphs_with`] draws.
#[derive(Clone, Copy, Debug)]
enum Par {
    /// In parallel when the patches hold [`PARALLEL_PX`] pixels or more.
    Auto,
    /// One patch after another on this thread.
    #[cfg_attr(not(test), allow(dead_code))]
    Serial,
    /// In parallel, rasterising at most this many patch pixels ahead of their blit.
    #[cfg_attr(not(test), allow(dead_code))]
    Batches(u64),
}

/// Draw glyphs (paths mapped by `to_px` from layer space into `img`).
fn draw_glyphs(geom: &TextGeom, img: &mut Image, to_px: &Mat3, s: f64, only: Option<usize>) {
    draw_glyphs_with(geom, img, to_px, s, only, Par::Auto);
}

/// [`draw_glyphs`]. In parallel, batches of patches are rasterised on every core (each on its
/// own, as serially), then blitted in parallel over bands of rows, each band applying every
/// patch in drawing order: every pixel gets the same blends in the same order, so the pixels are
/// the serial ones.
fn draw_glyphs_with(geom: &TextGeom, img: &mut Image, to_px: &Mat3, s: f64, only: Option<usize>, par: Par) {
    let (w, h) = (img.width as i64, img.height as i64);
    let runs = match only {
        Some(o) => passes(geom).into_iter().filter(|(gi, _)| *gi == o && geom.glyphs[*gi].xf.opacity > 0.0).map(|(gi, st)| (vec![gi], st)).collect(),
        None => runs(geom),
    };
    let mut jobs = Vec::with_capacity(runs.len());
    for (run, stroke) in runs {
        let Some(g) = run.first().and_then(|&gi| geom.glyphs.get(gi)) else { continue };
        // One glyph is drawn from its own outline; a run from its glyphs' outlines in layer
        // space, all wound the same way so overlaps add up instead of cancelling (NonZero).
        let local = Cow::Borrowed(std::slice::from_ref(&g.local));
        let (paths, m): (Cow<[BezPath]>, Mat3) = match (only, run.as_slice()) {
            (Some(_), _) => (local, *to_px),
            (None, [_]) => (local, *to_px * g.m2()),
            (None, _) => {
                let wound = |p: BezPath| if kurbo::Shape::area(&p) < 0.0 { p.reverse_subpaths() } else { p };
                (Cow::Owned(run.iter().filter_map(|&gi| geom.glyphs.get(gi)).map(|g| wound(g.path())).collect()), *to_px)
            }
        };
        let Some(b) = effectcraft_path::bounds(&paths) else { continue };
        let blur = [g.xf.blur[0] * s, g.xf.blur[1] * s];
        let pad = g.stroke_width * m.mean_scale() + 2.0 + blur[0].max(blur[1]) * 1.5;
        let r = m.map_rect(&effectcraft_geom::Rect::new(b.x0, b.y0, b.x1, b.y1));
        let rect = [
            ((r.x0 - pad).floor() as i64).max(0),
            ((r.y0 - pad).floor() as i64).max(0),
            ((r.x1 + pad).ceil() as i64).min(w),
            ((r.y1 + pad).ceil() as i64).min(h),
        ];
        if rect[2] <= rect[0] || rect[3] <= rect[1] {
            continue;
        }
        jobs.push(GlyphJob { g, stroke, paths, m, rect, blur });
    }
    let mode = if only.is_some() { BlendMode::Normal } else { geom.blend };
    let batch_px = match par {
        Par::Auto if jobs.iter().map(GlyphJob::area).fold(0u64, u64::saturating_add) >= PARALLEL_PX => BATCH_PX,
        Par::Batches(px) => px,
        Par::Auto | Par::Serial => 0,
    };
    if batch_px == 0 {
        for job in &jobs {
            blit(img, &job.raster(), [job.rect[0], job.rect[1]], mode);
        }
        return;
    }
    use rayon::prelude::*;
    let mut rest = jobs.as_slice();
    while !rest.is_empty() {
        // The next batch: jobs until their patches hold `batch_px` pixels (at least one).
        let mut n = 0;
        let mut px = 0u64;
        while let Some(job) = rest.get(n) {
            if n > 0 && px.saturating_add(job.area()) > batch_px {
                break;
            }
            px = px.saturating_add(job.area());
            n += 1;
        }
        let (batch, more) = rest.split_at(n.min(rest.len()));
        rest = more;
        let patches: Vec<(Image, [i64; 2])> = batch.par_iter().map(|job| (job.raster(), [job.rect[0], job.rect[1]])).collect();
        blit_rows(img, &patches, mode);
    }
}

/// [`blit`] of every patch in order, in parallel over bands of `dst`'s rows: each pixel gets the
/// patches covering it in the same order as by one [`blit`] after another.
fn blit_rows(dst: &mut Image, patches: &[(Image, [i64; 2])], mode: BlendMode) {
    use rayon::prelude::*;
    const BAND: usize = 8;
    let w = dst.width as usize;
    if w == 0 || patches.is_empty() {
        return;
    }
    dst.data.par_chunks_mut(w.saturating_mul(BAND)).enumerate().for_each(|(bi, band)| {
        let y0 = bi.saturating_mul(BAND) as i64;
        let y1 = y0 + (band.len() / w) as i64;
        for (src, at) in patches {
            let sw = src.width as i64;
            for y in (y0 - at[1]).max(0)..(y1 - at[1]).min(src.height as i64) {
                let row = (y + at[1] - y0) * w as i64;
                for x in 0..sw {
                    let tx = x + at[0];
                    if tx < 0 || tx >= w as i64 {
                        continue;
                    }
                    let Some(&s) = src.data.get((y * sw + x) as usize) else { continue };
                    if s[3] <= 0.0 && s[0] == 0.0 && s[1] == 0.0 && s[2] == 0.0 {
                        continue;
                    }
                    if let Some(d) = band.get_mut((row + tx) as usize) {
                        *d = effectcraft_color::blend_pixel(mode, *d, s, 0.5);
                    }
                }
            }
        }
    });
}

/// Render a text layer into a layer-space buffer at scale `s`.
pub fn render(ctx: &EvalCtx, layer: &Layer, s: f64) -> Buf {
    let empty = || Buf { img: Image::new(1, 1), offset: [0.0; 2], scale: s };
    let Some(geom) = text_geom(ctx, layer) else { return empty() };
    let mut bounds: Option<kurbo::Rect> = None;
    for g in &geom.glyphs {
        if let Some(b) = effectcraft_path::bounds(&[g.path()]) {
            let pad = g.stroke_width + 2.0 + g.xf.blur[0].max(g.xf.blur[1]) * 1.5;
            let b = b.inflate(pad, pad);
            bounds = Some(bounds.map_or(b, |a| a.union(b)));
        }
    }
    let Some(b) = bounds else { return empty() };
    let b = b.intersect(kurbo::Rect::new(-20000.0, -20000.0, 20000.0, 20000.0));
    // Keep the layer pixel grid: a fractional buffer origin would blur even an integer placement.
    let (x0, y0) = ((b.x0 * s).floor(), (b.y0 * s).floor());
    let w = ((b.x1 * s).ceil() - x0 + 4.0).clamp(1.0, 16384.0) as u32;
    let h = ((b.y1 * s).ceil() - y0 + 4.0).clamp(1.0, 16384.0) as u32;
    let offset = [-x0 + 2.0, -y0 + 2.0];
    let mut img = Image::new(w, h);
    let base = Mat3::translate(vec2(offset[0], offset[1])) * Mat3::scale(vec2(s, s));
    draw_glyphs(&geom, &mut img, &base, s, None);
    Buf { img, offset, scale: s }
}

/// Per-character 3D: each character as its own plane — (buffer in character space, character
/// space → layer space matrix).
pub fn per_char_planes(ctx: &EvalCtx, layer: &Layer, s: f64) -> Vec<(Buf, Mat4)> {
    use rayon::prelude::*;
    let Some(geom) = text_geom(ctx, layer) else { return vec![] };
    // Each character is drawn into its own buffer: in parallel, collected in order.
    geom.glyphs
        .par_iter()
        .enumerate()
        .filter_map(|(gi, g)| {
            if g.xf.opacity <= 0.0 {
                return None;
            }
            let b = effectcraft_path::bounds(std::slice::from_ref(&g.local))?;
            let pad = g.stroke_width + 2.0 + g.xf.blur[0].max(g.xf.blur[1]) * 1.5;
            let b = b.inflate(pad, pad);
            let w = ((b.width() * s).ceil() as u32 + 4).clamp(1, 4096);
            let h = ((b.height() * s).ceil() as u32 + 4).clamp(1, 4096);
            let offset = [-b.x0 * s + 2.0, -b.y0 * s + 2.0];
            let mut img = Image::new(w, h);
            let base = Mat3::translate(vec2(offset[0], offset[1])) * Mat3::scale(vec2(s, s));
            draw_glyphs(&geom, &mut img, &base, s, Some(gi));
            Some((Buf { img, offset, scale: s }, g.m))
        })
        .collect()
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_gradient;
