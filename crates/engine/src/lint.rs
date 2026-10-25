//! Design lint: plain-language design problems in a comp at a time or over a time range
//! (`render.lint`, MCP `lint_frame`). Headless agents can't judge a frame by eye; these rules
//! catch the usual mistakes and say how to fix them. The command (sampling, rendering, audio) is
//! `commands/lint_cmds.rs`; the rules are here.
//!
//! The rules ([`RULES`]), checked at each sample time on the comp's own layers (precomps are
//! one layer each, except for the nested rules; track-matte sources and adjustment layers are
//! skipped):
//! - `text-too-small`: the smallest drawn character's font size in comp pixels (the em size
//!   carried through the character's animator transform, the layer's transform, its parents and
//!   the camera: what a "72 px" font becomes on screen; cap height is about 70 % of it) is below
//!   [`Settings::min_text_px`] (default 1/30 of the comp height, 36 px at 1080p): warning; below
//!   half of it: error.
//! - `low-contrast`: the WCAG 2 contrast ratio between the text colour (mean of the drawn
//!   glyphs' fills, blended with what is behind by the layer's opacity) and the mean colour under
//!   the text's box in a render of the comp without its text layers ([`Backdrop`]) is below
//!   [`Settings::min_contrast`] (default 3:1): warning; below two thirds of it (2:1): error.
//! - `outside-title-safe` (text, warning) / `outside-action-safe` (other layers, info): part of
//!   the layer is outside the title-safe / action-safe rectangle (Settings ▸ Grids & Guides, After
//!   Effects' defaults: 10 % and 5 % of the frame per side).
//! - `cut-off`: part, not all, of the layer's content is outside the frame: warning for text,
//!   info for other layers. Layers that cover the whole frame (backgrounds) are skipped.
//! - `text-overlap`: two visible text layers' boxes overlap by more than 2 % of the smaller one
//!   (warning; `other` is the second layer).
//! - `text-off-centre`: a text layer sits in a card (the smallest drawn non-text layer below it
//!   whose box holds the text's, not a full-frame background) and its glyph box's centre is more
//!   than 1.5 px off the card's centre, measured along the card's own axes: vertically when the
//!   card is at most 3 text heights tall (a pill or one-line card), horizontally when the text
//!   is centre-justified. Warning; `other` is the card.
//! - `nested-cut-off` ([`lint_nested`]): a layer inside a precomp, measured in this comp's frame
//!   through the precomp layers' transforms, is partly outside this comp's frame, or partly
//!   outside its precomp's frame (which clips it unless Collapse Transformations is on).
//!   Warning for text, info for other layers; the nested layer is [`Finding::inner`]. Text
//!   inside precomps also gets `text-too-small` and `outside-title-safe`, measured the same way.
//! - `exposed-edge`: with the comp rendered (only at times its layers' boxes claim 90 % of the
//!   frame, so overlay comps are skipped), an empty strip of at least 1 px along a frame edge
//!   while at most a quarter of the frame is empty (a full-screen shot scaled or shaken off the
//!   frame), or 1 % or more of the frame empty between two fully covered samples (a gap in a
//!   transition). Warning, on the layer meant to fill the frame.
//! - `hidden-visible`: a layer with its Video switch on, inside its in/out range, that shows
//!   nothing: opacity 0, entirely outside the frame, or entirely covered by an opaque layer above
//!   it (simple cases only: a solid, or footage whose alpha is ignored, at 100 % opacity, Normal
//!   mode, 2D, without masks, effects or a track matte). Info.
//! - `off-beat`: with audio, a key moment (a transform or opacity keyframe, or a layer's in
//!   point after the comp start) within half a beat of a beat that misses it by more than one
//!   frame. Beats come from [`detect_beats`] on the comp's audio mix. Warning.
//! - `off-accent`: with a speech map (`speech.import`), a keyframed property's speed-graph peak
//!   ([`speed_peaks_deep`]: also inside precomps) within a few frames (default 4) of a stressed syllable's accent
//!   ([`accent_time`]: one frame before its vowel, never before its onset) that misses it by
//!   more than one frame. Warning.

use effectcraft_geom::{Mat3, vec2};
use effectcraft_project::{AlphaMode, Comp, ItemId, ItemKind, Layer, LayerId, LayerSource, Project};
use effectcraft_raster::Image;
use effectcraft_render::{EvalCtx, Renderer};
use effectcraft_time::Tick;

use crate::layout;
use crate::precomp_time::{self, Hop};

/// Every rule id, in report order.
pub const RULES: [&str; 12] = [
    "text-too-small",
    "low-contrast",
    "outside-title-safe",
    "outside-action-safe",
    "cut-off",
    "nested-cut-off",
    "text-overlap",
    "text-off-centre",
    "hidden-visible",
    "exposed-edge",
    "off-beat",
    "off-accent",
];

/// Most sample times one lint looks at; longer ranges are thinned evenly (`truncated`).
pub const MAX_SAMPLES: usize = 120;
/// Default spacing of range samples (seconds).
pub const DEFAULT_STEP: f64 = 0.5;
/// Longest side of the backdrop render `low-contrast` measures (pixels).
pub const BACKDROP_SIDE: f64 = 480.0;
/// Text boxes overlapping by more than this share of the smaller box collide.
const OVERLAP_SHARE: f64 = 0.02;
/// Shares within this of 0 or 1 count as none or all.
const EPS: f64 = 1e-3;
/// Most regular samples generated before thinning (bounds the work a tiny `step` asks for).
const MAX_CANDIDATES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }
}

/// One problem at one time.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    pub layer: LayerId,
    /// The second layer of a pair rule (`text-overlap`, the cover of `hidden-visible`, the card of
    /// `text-off-centre`).
    pub other: Option<LayerId>,
    /// The layer inside a precomp the finding is about (`layer` is then the precomp layer of
    /// the linted comp that shows it).
    pub inner: Option<Inner>,
    pub message: String,
}

/// A layer inside a precomp, measured in the linted comp's frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Inner {
    /// The comp the layer is in (the innermost precomp).
    pub comp: ItemId,
    pub layer: LayerId,
    /// "Phone › Screen › Box": the precomp layers down to the layer.
    pub path: String,
}

/// What a lint checks and its thresholds.
#[derive(Clone, Debug)]
pub struct Settings {
    pub rules: Vec<&'static str>,
    /// Smallest legible font size on screen (comp pixels).
    pub min_text_px: f64,
    /// Smallest acceptable WCAG contrast ratio of text against its background.
    pub min_contrast: f64,
    /// Title-safe and action-safe margins in percent of the frame, both sides together (as in
    /// Settings ▸ Grids & Guides: 20 = 10 % per side).
    pub title_safe: f64,
    pub action_safe: f64,
}

impl Settings {
    /// Every rule with the default thresholds for `comp`.
    pub fn new(comp: &Comp) -> Settings {
        Settings { rules: RULES.to_vec(), min_text_px: comp.height as f64 / 30.0, min_contrast: 3.0, title_safe: 20.0, action_safe: 10.0 }
    }
    pub fn on(&self, rule: &str) -> bool {
        self.rules.contains(&rule)
    }
}

/// The comp rendered without its text layers, for measuring what text sits on.
pub struct Backdrop {
    /// Premultiplied render at `scale` × comp pixels.
    pub img: Image,
    pub scale: f64,
    /// The comp's background colour (what transparent pixels show).
    pub background: [f32; 3],
}

impl Backdrop {
    /// Mean colour (display values 0–1, composited over the background) inside the comp-space
    /// rectangle `[x, y, w, h]`; `None` when it misses the frame.
    pub fn mean(&self, r: [f64; 4]) -> Option<[f64; 3]> {
        let (w, h) = (self.img.width as usize, self.img.height as usize);
        let px = |v: f64, max: usize| if v.is_finite() { v.clamp(0.0, max as f64) as usize } else { 0 };
        let (x0, x1) = (px((r[0] * self.scale).floor(), w), px(((r[0] + r[2]) * self.scale).ceil(), w));
        let (y0, y1) = (px((r[1] * self.scale).floor(), h), px(((r[1] + r[3]) * self.scale).ceil(), h));
        let (mut sum, mut n) = ([0.0f64; 3], 0usize);
        for y in y0..y1 {
            for x in x0..x1 {
                let Some(p) = self.img.data.get(y * w + x) else { continue };
                let k = 1.0 - p[3].clamp(0.0, 1.0);
                for (c, s) in sum.iter_mut().enumerate() {
                    *s += (p[c] + self.background[c] * k).clamp(0.0, 1.0) as f64;
                }
                n += 1;
            }
        }
        (n > 0).then(|| sum.map(|s| s / n as f64))
    }
}

/// `project` with the text layers of `comp` switched off: what the text sits on.
pub fn without_text(project: &Project, comp: ItemId) -> Option<Project> {
    let mut p = project.clone();
    for l in &mut p.comp_mut(comp)?.layers {
        if matches!(l.source, LayerSource::Text) {
            l.switches.video = false;
        }
    }
    Some(p)
}

/// Whether a text layer is drawn at the context time (a backdrop is worth rendering).
pub fn any_text_visible(ctx: &EvalCtx) -> bool {
    ctx.comp.layers.iter().any(|l| matches!(l.source, LayerSource::Text) && layout::visibility(ctx, l).0)
}

/// Whether the layer has pixels of its own (not a null, camera, light or audio-only footage).
fn has_picture(project: &Project, l: &Layer) -> bool {
    match &l.source {
        LayerSource::Footage { item } => !matches!(project.item(*item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if !f.has_video),
        s => s.is_av(),
    }
}

/// A drawn layer's place on screen at one time.
struct Placed<'a> {
    layer: &'a Layer,
    text: bool,
    corners: [[f64; 2]; 4],
    bounds: [f64; 4],
    area: f64,
    on_screen: f64,
    /// Covers the whole frame (a background).
    full_frame: bool,
}

fn name(l: &Layer) -> String {
    format!("“{}”", l.name)
}

fn hex(c: [f64; 3]) -> String {
    let b = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c[0]), b(c[1]), b(c[2]))
}

/// WCAG 2 relative luminance of a display (sRGB-encoded) colour.
pub fn luminance(c: [f64; 3]) -> f64 {
    let lin = |v: f64| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG 2 contrast ratio (1–21) of two colours.
pub fn contrast_ratio(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Text measurements at the context time: the smallest drawn character's font size in comp
/// pixels with its nominal size, and the text colour (mean of the drawn fills, or of the strokes
/// when nothing is filled; straight RGBA).
struct TextLook {
    px: f64,
    size: f64,
    color: Option<[f64; 4]>,
}

/// A point of `ctx`'s comp in the linted comp: through `outer`, the precomp layers' layer → comp
/// matrices from the innermost out (empty when `ctx` is the linted comp itself).
fn through(outer: &[Mat3], p: [f64; 2]) -> ([f64; 2], bool) {
    outer.iter().fold((p, true), |(q, ok), m| {
        let (r, ok2) = layout::to_comp(m, q);
        (r, ok && ok2)
    })
}

fn text_look(ctx: &EvalCtx, l: &Layer, outer: &[Mat3]) -> Option<TextLook> {
    let geom = effectcraft_render::text::text_geom(ctx, l)?;
    let (m, _) = ctx.layer_to_comp(l);
    let to_linted = |p: [f64; 2]| {
        let (q, ok) = layout::to_comp(&m, p);
        let (r, ok2) = through(outer, q);
        (r, ok && ok2)
    };
    let (mut px, mut size) = (f64::INFINITY, 0.0);
    let (mut fill, mut nf, mut stroke, mut ns) = ([0.0f64; 4], 0usize, [0.0f64; 4], 0usize);
    for g in &geom.glyphs {
        let filled = g.apply_fill && g.fill[3] > 0.0;
        let stroked = g.stroke_width > 0.0 && g.stroke[3] > 0.0;
        if g.local.is_empty() || !(filled || stroked) {
            continue;
        }
        let style = geom.doc.style_at(g.char_index);
        let em = style.size * style.v_scale / 100.0;
        let m2 = g.m2();
        let (a, b) = (m2.apply(vec2(0.0, 0.0)), m2.apply(vec2(0.0, em)));
        let ((a, oka), (b, okb)) = (to_linted([a.x, a.y]), to_linted([b.x, b.y]));
        let d = (b[0] - a[0]).hypot(b[1] - a[1]);
        if oka && okb && d.is_finite() && d < px {
            px = d;
            size = style.size;
        }
        if filled {
            fill.iter_mut().zip(g.fill).for_each(|(s, v)| *s += v as f64);
            nf += 1;
        } else {
            stroke.iter_mut().zip(g.stroke).for_each(|(s, v)| *s += v as f64);
            ns += 1;
        }
    }
    if !px.is_finite() {
        return None;
    }
    let color = if nf > 0 {
        Some(fill.map(|v| v / nf as f64))
    } else if ns > 0 {
        Some(stroke.map(|v| v / ns as f64))
    } else {
        None
    };
    Some(TextLook { px, size, color })
}

/// The comp-space rectangle `[x0, y0, x1, y1]` inset by `pct` % of the frame (both sides).
fn safe_rect(w: f64, h: f64, pct: f64) -> [f64; 4] {
    let (dx, dy) = (w * pct / 200.0, h * pct / 200.0);
    [dx, dy, w - dx, h - dy]
}

/// Share (0–1) of the quad inside the rectangle `[x0, y0, x1, y1]`.
fn inside_share(corners: &[[f64; 2]; 4], area: f64, r: [f64; 4]) -> f64 {
    let rect = [[r[0], r[1]], [r[2], r[1]], [r[2], r[3]], [r[0], r[3]]];
    if area <= 1e-9 {
        return 0.0;
    }
    (layout::overlap_area(corners, &rect) / area).clamp(0.0, 1.0)
}

/// "42 px on the left and 10 px at the top": how far `bounds` reaches past `r` per side.
fn overshoot(bounds: [f64; 4], r: [f64; 4]) -> String {
    let sides = [
        (r[0] - bounds[0], "on the left"),
        (bounds[0] + bounds[2] - r[2], "on the right"),
        (r[1] - bounds[1], "at the top"),
        (bounds[1] + bounds[3] - r[3], "at the bottom"),
    ];
    let parts: Vec<String> = sides.iter().filter(|(d, _)| *d >= 0.5).map(|(d, side)| format!("{d:.0} px {side}")).collect();
    match parts.len() {
        0 => "slightly".into(),
        1 => parts.concat(),
        _ => {
            let (last, first) = parts.split_last().map(|(l, f)| (l.clone(), f.join(", "))).unwrap_or_default();
            format!("{first} and {last}")
        }
    }
}

/// "the left and top edges": which frame edges `bounds` crosses.
fn edges(bounds: [f64; 4], w: f64, h: f64) -> String {
    let e: Vec<&str> =
        [(bounds[0] < -0.5, "left"), (bounds[0] + bounds[2] > w + 0.5, "right"), (bounds[1] < -0.5, "top"), (bounds[1] + bounds[3] > h + 0.5, "bottom")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, s)| *s)
            .collect();
    match e.as_slice() {
        [] => "frame edge".into(),
        [one] => format!("{one} frame edge"),
        [a, b] => format!("{a} and {b} frame edges"),
        more => format!("{} frame edges", more.join(", ")),
    }
}

/// A layer above that hides everything under its box: a solid (or footage whose alpha is
/// ignored) at 100 % opacity in Normal mode, 2D, without masks, effects, a track matte, layer
/// styles or Preserve Transparency.
fn opaque_cover(ctx: &EvalCtx, l: &Layer) -> bool {
    let opaque_source = match &l.source {
        LayerSource::Solid { .. } => true,
        LayerSource::Footage { item } => {
            matches!(ctx.project.item(*item).map(|i| &i.kind), Some(ItemKind::Footage(f)) if f.alpha == AlphaMode::Ignore && f.has_video)
        }
        _ => false,
    };
    let empty = |g: Option<&effectcraft_project::PropGroup>| g.is_none_or(|g| g.children.is_empty());
    opaque_source
        && !l.is_3d()
        && l.blend_mode == Default::default()
        && l.track_matte.is_none()
        && !l.preserve_transparency
        && empty(l.masks())
        && (empty(l.effects()) || !l.switches.effects)
        && empty(l.props.sub("layerStyles"))
        && ctx.opacity(l) >= 1.0 - 1e-6
}

/// Text off its card's centre by more than this (comp pixels) is flagged.
const CENTRE_TOL: f64 = 1.5;
/// Vertical centring is checked when the card is at most this many times the text's height
/// (a pill or a one-line card; text in a tall card is usually placed on purpose).
const CENTRE_HEIGHTS: f64 = 3.0;

/// How far the centre of the text's points (the corners of one or more text boxes) is from
/// the card quad's centre along the card's own axes (`[right, down]` in comp pixels, so rotated
/// cards work), for each axis that is checked: down when the card is at most
/// [`CENTRE_HEIGHTS`] times the text's height, right when the text is centre-justified. `None`
/// when nothing checked is off by more than [`CENTRE_TOL`].
fn off_centre(text: &[[f64; 2]], card: &[[f64; 2]; 4], check_h: bool) -> Option<[Option<f64>; 2]> {
    let sub = |a: [f64; 2], b: [f64; 2]| [a[0] - b[0], a[1] - b[1]];
    let dot = |a: [f64; 2], b: [f64; 2]| a[0] * b[0] + a[1] * b[1];
    let (u, v) = (sub(card[1], card[0]), sub(card[3], card[0]));
    let (lu, lv) = (dot(u, u).sqrt(), dot(v, v).sqrt());
    if lu <= 1e-6 || lv <= 1e-6 || text.is_empty() {
        return None;
    }
    // The text's extent along an axis from the card's first corner: (centre, length).
    let along = |axis: [f64; 2], len: f64| {
        let axis = [axis[0] / len, axis[1] / len];
        let (lo, hi) = text.iter().map(|p| dot(sub(*p, card[0]), axis)).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), x| (lo.min(x), hi.max(x)));
        ((lo + hi) / 2.0 - len / 2.0, hi - lo)
    };
    let ((dx, _), (dy, h)) = (along(u, lu), along(v, lv));
    let check_v = lv <= h * CENTRE_HEIGHTS;
    let pick = |on: bool, x: f64| (on && x.is_finite() && x.abs() > CENTRE_TOL).then_some(x);
    let off = [pick(check_h, dx), pick(check_v, dy)];
    (off[0].is_some() || off[1].is_some()).then_some(off)
}

/// "“Price” is 5.5 px below and 2.0 px right of the centre of its card “Pill”: …".
fn off_centre_message(text: &str, several: bool, card: &str, off: [Option<f64>; 2]) -> String {
    let parts: Vec<(String, String)> = [
        off[1].map(|y| (format!("{:.1} px {}", y.abs(), if y > 0.0 { "below" } else { "above" }), format!("{:.1} px {}", y.abs(), if y > 0.0 { "up" } else { "down" }))),
        off[0].map(|x| {
            (format!("{:.1} px {} of", x.abs(), if x > 0.0 { "right" } else { "left" }), format!("{:.1} px {}", x.abs(), if x > 0.0 { "left" } else { "right" }))
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let is: Vec<&str> = parts.iter().map(|p| p.0.as_str()).collect();
    let fix: Vec<&str> = parts.iter().map(|p| p.1.as_str()).collect();
    let (verb, them) = if several { ("are", "them") } else { ("is", text) };
    format!(
        "{text} {verb} {} the centre of {} card {card} (measured on the drawn glyphs, along the card's own axes): move {them} {} so {} centred in the card.",
        is.join(" and "),
        if several { "their" } else { "its" },
        fix.join(" and "),
        if several { "they sit" } else { "it sits" }
    )
}

/// Every enabled rule (except `off-beat` and `off-accent`, which look at the whole span) at the context time.
/// `backdrop` (the comp without text) is needed for `low-contrast`.
pub fn lint_at(ctx: &EvalCtx, set: &Settings, backdrop: Option<&Backdrop>) -> Vec<Finding> {
    let comp = ctx.comp;
    let (w, h) = (comp.width as f64, comp.height as f64);
    let frame = [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]];
    let mattes: Vec<LayerId> = comp.layers.iter().filter_map(|l| l.track_matte.map(|m| m.layer)).collect();
    let mut out = vec![];
    let mut push = |rule: &'static str, severity: Severity, l: &Layer, other: Option<LayerId>, message: String| {
        out.push(Finding { rule, severity, layer: l.id, other, inner: None, message });
    };
    let mut placed: Vec<Placed> = vec![];
    for l in &comp.layers {
        if mattes.contains(&l.id) || l.switches.adjustment || !has_picture(ctx.project, l) {
            continue;
        }
        let (visible, why) = layout::visibility(ctx, l);
        if !visible {
            if set.on("hidden-visible") && why == ["opacity is 0"] {
                push(
                    "hidden-visible",
                    Severity::Info,
                    l,
                    None,
                    format!(
                        "{} is switched on and inside its in/out range but its opacity is 0, so nothing of it shows: raise its Opacity, or trim the layer to when it should be seen.",
                        name(l)
                    ),
                );
            }
            continue;
        }
        let Some(g) = layout::layer_geometry(ctx, l).filter(|g| g.projected) else { continue };
        let area = layout::polygon_area(&g.corners);
        if area <= 1e-9 {
            continue;
        }
        let on_screen = layout::on_screen_fraction(&g.corners, w, h);
        let full_frame = layout::overlap_area(&g.corners, &frame) >= w * h * (1.0 - EPS);
        placed.push(Placed { layer: l, text: matches!(l.source, LayerSource::Text), corners: g.corners, bounds: g.bounds, area, on_screen, full_frame });
    }
    let title = safe_rect(w, h, set.title_safe);
    let action = safe_rect(w, h, set.action_safe);
    for (i, p) in placed.iter().enumerate() {
        let l = p.layer;
        if p.on_screen <= EPS {
            if set.on("hidden-visible") {
                let b = p.bounds.map(|v| v.round());
                push(
                    "hidden-visible",
                    Severity::Info,
                    l,
                    None,
                    format!(
                        "{} is switched on and inside its in/out range but entirely outside the {}×{} frame (its box is at [{}, {}, {}, {}]): move it into view, or trim it or switch it off if it shouldn't be seen here.",
                        name(l),
                        comp.width,
                        comp.height,
                        b[0],
                        b[1],
                        b[2],
                        b[3]
                    ),
                );
            }
            continue;
        }
        if set.on("hidden-visible")
            && let Some(cover) =
                placed.iter().take(i).find(|a| opaque_cover(ctx, a.layer) && layout::overlap_area(&p.corners, &a.corners) >= p.area * (1.0 - EPS))
        {
            push(
                "hidden-visible",
                Severity::Info,
                l,
                Some(cover.layer.id),
                format!(
                    "{} is entirely covered by {}, an opaque layer above it, so nothing of it shows: move it above {} in the layer stack, or shrink, move or fade the covering layer.",
                    name(l),
                    name(cover.layer),
                    name(cover.layer)
                ),
            );
        }
        if p.text {
            let look = if set.on("text-too-small") || set.on("low-contrast") { text_look(ctx, l, &[]) } else { None };
            if set.on("text-too-small")
                && let Some(t) = &look
                && t.px < set.min_text_px
            {
                let severity = if t.px < set.min_text_px / 2.0 { Severity::Error } else { Severity::Warning };
                let scale = if (t.px - t.size).abs() > 0.5 { format!(" (font size {:.0} at the layer's scale)", t.size) } else { String::new() };
                push(
                    "text-too-small",
                    severity,
                    l,
                    None,
                    format!(
                        "{}'s smallest characters are {:.1} px on screen{scale}, below the {:.0} px minimum for legible text in a {} px tall frame{}: increase the font size or the layer's scale.",
                        name(l),
                        t.px,
                        set.min_text_px,
                        comp.height,
                        if severity == Severity::Error { " (less than half of it)" } else { "" }
                    ),
                );
            }
            if set.on("low-contrast")
                && let (Some(t), Some(bd)) = (&look, backdrop)
                && let Some(c) = t.color
                && let Some(bg) = bd.mean(p.bounds)
            {
                let a = (c[3] * ctx.opacity(l)).clamp(0.0, 1.0);
                if a > EPS {
                    let fg = [0, 1, 2].map(|k| bg[k] * (1.0 - a) + c[k] * a);
                    let ratio = contrast_ratio(fg, bg);
                    if ratio < set.min_contrast {
                        let severity = if ratio < set.min_contrast * 2.0 / 3.0 { Severity::Error } else { Severity::Warning };
                        push(
                            "low-contrast",
                            severity,
                            l,
                            None,
                            format!(
                                "{}'s text colour {} on its background (about {} under the text) has a contrast ratio of {:.2}:1, below {:.1}:1: make the text lighter or darker, change what is behind it, or add a stroke or shadow.",
                                name(l),
                                hex(fg),
                                hex(bg),
                                ratio,
                                set.min_contrast
                            ),
                        );
                    }
                }
            }
            if set.on("outside-title-safe") && inside_share(&p.corners, p.area, title) < 1.0 - EPS {
                push(
                    "outside-title-safe",
                    Severity::Warning,
                    l,
                    None,
                    format!(
                        "{} reaches {} past the title-safe area (text should stay {:.0} px from the left and right and {:.0} px from the top and bottom, {}% of the frame per side): move it inward or make it smaller.",
                        name(l),
                        overshoot(p.bounds, title),
                        title[0],
                        title[1],
                        set.title_safe / 2.0
                    ),
                );
            }
        } else if set.on("outside-action-safe") && !p.full_frame && inside_share(&p.corners, p.area, action) < 1.0 - EPS {
            push(
                "outside-action-safe",
                Severity::Info,
                l,
                None,
                format!(
                    "{} reaches {} past the action-safe area ({}% of the frame per side), which some displays crop: keep important content inside it.",
                    name(l),
                    overshoot(p.bounds, action),
                    set.action_safe / 2.0
                ),
            );
        }
        if set.on("cut-off") && !p.full_frame && p.on_screen < 1.0 - EPS {
            push(
                "cut-off",
                if p.text { Severity::Warning } else { Severity::Info },
                l,
                None,
                format!(
                    "{} is cut off by the {}: {:.0}% of it is outside the frame. Move it inside or make it smaller{}.",
                    name(l),
                    edges(p.bounds, w, h),
                    (1.0 - p.on_screen) * 100.0,
                    if p.text { "" } else { " if it should be seen whole" }
                ),
            );
        }
    }
    if set.on("text-off-centre") {
        let texts: Vec<(usize, &Placed)> = placed.iter().enumerate().filter(|(_, p)| p.text && p.on_screen > EPS).collect();
        // The card of a text: the smallest drawn layer below it whose box holds the text's.
        let card_of = |i: usize, p: &Placed| {
            placed
                .iter()
                .enumerate()
                .skip(i + 1)
                .filter(|(_, c)| !c.text && !c.full_frame && c.area > p.area && layout::overlap_area(&p.corners, &c.corners) >= p.area * (1.0 - EPS))
                .min_by(|a, b| a.1.area.total_cmp(&b.1.area))
                .map(|(j, _)| j)
        };
        let cards: Vec<Option<usize>> = texts.iter().map(|(i, p)| card_of(*i, p)).collect();
        let mut done: Vec<usize> = vec![];
        for (k, card) in cards.iter().enumerate() {
            let Some(card) = *card else { continue };
            if done.contains(&card) {
                continue;
            }
            done.push(card);
            let Some(c) = placed.get(card) else { continue };
            // Every text on the card is placed as one block (a name over a role).
            let group: Vec<&Placed> = texts.iter().zip(&cards).skip(k).filter(|(_, cc)| **cc == Some(card)).map(|(t, _)| t.1).collect();
            let centred = |p: &Placed| {
                effectcraft_render::text::text_geom(ctx, p.layer)
                    .is_some_and(|g| matches!(g.doc.justify, effectcraft_keyframe::Justify::Center | effectcraft_keyframe::Justify::JustifyLastCenter))
            };
            let pts: Vec<[f64; 2]> = group.iter().flat_map(|p| p.corners).collect();
            let Some(off) = off_centre(&pts, &c.corners, group.iter().all(|p| centred(p))) else { continue };
            let Some(first) = group.first() else { continue };
            let subject = match group.as_slice() {
                [one] => name(one.layer),
                more => format!("{} (together)", more.iter().map(|p| name(p.layer)).collect::<Vec<_>>().join(" and ")),
            };
            push("text-off-centre", Severity::Warning, first.layer, Some(c.layer.id), off_centre_message(&subject, group.len() > 1, &name(c.layer), off));
        }
    }
    if set.on("text-overlap") {
        let texts: Vec<&Placed> = placed.iter().filter(|p| p.text && p.on_screen > EPS).collect();
        for (i, a) in texts.iter().enumerate() {
            for b in texts.iter().skip(i + 1) {
                let shared = layout::overlap_area(&a.corners, &b.corners);
                let smaller = a.area.min(b.area);
                if smaller > 1e-9 && shared > smaller * OVERLAP_SHARE {
                    push(
                        "text-overlap",
                        Severity::Warning,
                        a.layer,
                        Some(b.layer.id),
                        format!(
                            "{} and {} overlap: {:.0}% of the smaller one's box is covered. Move one of them or make them smaller so the text doesn't collide.",
                            name(a.layer),
                            name(b.layer),
                            shared / smaller * 100.0
                        ),
                    );
                }
            }
        }
    }
    out
}

/// How many precomps deep [`lint_nested`] looks (a precomp in a precomp in a precomp…).
pub const MAX_NEST: usize = 4;

/// Layers inside the comp's precomps, measured in the linted comp's frame (agents that look
/// inside a precomp measure in the precomp's frame and miss what its layer's scale, position
/// and edge do to it): `nested-cut-off` (part of the layer is outside the linted comp's frame,
/// or outside the precomp's own frame, which clips it unless Collapse Transformations is on),
/// and `text-too-small` and `outside-title-safe` for text inside precomps. Up to
/// [`MAX_NEST`] levels deep. Findings name the precomp layer of the linted comp as `layer` and
/// the nested layer as [`Finding::inner`].
pub fn lint_nested(r: &Renderer, ctx: &EvalCtx, set: &Settings) -> Vec<Finding> {
    let mut out = vec![];
    if !(set.on("nested-cut-off") || set.on("text-too-small") || set.on("outside-title-safe")) {
        return out;
    }
    let comp = ctx.comp;
    let mattes: Vec<LayerId> = comp.layers.iter().filter_map(|l| l.track_matte.map(|m| m.layer)).collect();
    for l in &comp.layers {
        let LayerSource::Comp { item } = l.source else { continue };
        if mattes.contains(&l.id) || l.switches.adjustment || !layout::visibility(ctx, l).0 {
            continue;
        }
        let (m, _) = ctx.layer_to_comp(l);
        let walk = Walk { r, set, top: l, w: comp.width as f64, h: comp.height as f64 };
        walk.precomp(ctx, l, item, &[m], &l.name, 1, &mut out);
    }
    out
}

struct Walk<'a, 'r> {
    r: &'a Renderer<'r>,
    set: &'a Settings,
    /// The precomp layer of the linted comp the walk is under.
    top: &'a Layer,
    /// The linted comp's size.
    w: f64,
    h: f64,
}

impl Walk<'_, '_> {
    /// The layers of `item`, shown by `host` (a layer of `outer_ctx`'s comp); `chain` maps
    /// `item`'s pixels into the linted comp (innermost first), `path` names the precomp layers.
    #[allow(clippy::too_many_arguments)]
    fn precomp(&self, outer_ctx: &EvalCtx, host: &Layer, item: ItemId, chain: &[Mat3], path: &str, depth: usize, out: &mut Vec<Finding>) {
        let Some(nctx) = self.r.eval_ctx(item, outer_ctx.nested_time(host)) else { return };
        let nc = nctx.comp;
        let (nw, nh) = (nc.width as f64, nc.height as f64);
        let nframe = [[0.0, 0.0], [nw, 0.0], [nw, nh], [0.0, nh]];
        let mattes: Vec<LayerId> = nc.layers.iter().filter_map(|l| l.track_matte.map(|m| m.layer)).collect();
        let title = safe_rect(self.w, self.h, self.set.title_safe);
        for il in &nc.layers {
            if mattes.contains(&il.id) || il.switches.adjustment || !has_picture(nctx.project, il) || !layout::visibility(&nctx, il).0 {
                continue;
            }
            let here = format!("{path} › {}", il.name);
            if let LayerSource::Comp { item: deeper } = il.source {
                if depth < MAX_NEST {
                    let (m, _) = nctx.layer_to_comp(il);
                    let mut next = Vec::with_capacity(chain.len() + 1);
                    next.push(m);
                    next.extend_from_slice(chain);
                    self.precomp(&nctx, il, deeper, &next, &here, depth + 1, out);
                }
                continue;
            }
            let Some(g) = layout::layer_geometry(&nctx, il).filter(|g| g.projected) else { continue };
            let inner_area = layout::polygon_area(&g.corners);
            // Backgrounds of the precomp (they fill its frame) are its frame, not content.
            if inner_area <= 1e-9 || layout::overlap_area(&g.corners, &nframe) >= nw * nh * (1.0 - EPS) {
                continue;
            }
            let mut ok = true;
            let corners = g.corners.map(|p| {
                let (q, k) = through(chain, p);
                ok &= k;
                q
            });
            let area = layout::polygon_area(&corners);
            if !ok || area <= 1e-9 {
                continue;
            }
            let text = matches!(il.source, LayerSource::Text);
            let inner = Inner { comp: item, layer: il.id, path: here.clone() };
            let shown = format!("“{here}”");
            let mut push = |rule: &'static str, severity: Severity, message: String| {
                out.push(Finding { rule, severity, layer: self.top.id, other: None, inner: Some(inner.clone()), message });
            };
            let in_precomp = inside_share(&g.corners, inner_area, [0.0, 0.0, nw, nh]);
            if in_precomp <= EPS && !host.switches.collapse {
                continue;
            }
            if self.set.on("nested-cut-off") {
                let on_screen = layout::on_screen_fraction(&corners, self.w, self.h);
                if !host.switches.collapse && in_precomp < 1.0 - EPS {
                    push(
                        "nested-cut-off",
                        if text { Severity::Warning } else { Severity::Info },
                        format!(
                            "{shown} is cut off by the edge of the precomp “{}” ({}×{}): {:.0}% of it is outside that comp's frame, which clips it here. Move it inside the precomp's frame, make the precomp bigger, or switch on Collapse Transformations on “{}”.",
                            nc_name(nctx.project, item),
                            nc.width,
                            nc.height,
                            (1.0 - in_precomp) * 100.0,
                            host.name
                        ),
                    );
                } else if on_screen > EPS && on_screen < 1.0 - EPS {
                    let b = layout::aabb(&corners);
                    push(
                        "nested-cut-off",
                        if text { Severity::Warning } else { Severity::Info },
                        format!(
                            "{shown} (inside the precomp “{}”) is cut off by the {} of this comp: {:.0}% of it is outside the frame once the precomp layer's position and scale are applied, though it looks whole inside the precomp. Move or scale it (or the precomp layer) so it is inside the frame{}.",
                            self.top.name,
                            edges(b, self.w, self.h),
                            (1.0 - on_screen) * 100.0,
                            if text { "" } else { " if it should be seen whole" }
                        ),
                    );
                }
            }
            if !text {
                continue;
            }
            if self.set.on("text-too-small")
                && let Some(t) = text_look(&nctx, il, chain)
                && t.px < self.set.min_text_px
            {
                let severity = if t.px < self.set.min_text_px / 2.0 { Severity::Error } else { Severity::Warning };
                push(
                    "text-too-small",
                    severity,
                    format!(
                        "{shown}'s smallest characters are {:.1} px on screen in this comp (font size {:.0} inside the precomp, made smaller by the precomp layers' scale), below the {:.0} px minimum for legible text in a {} px tall frame{}: increase the font size or the precomp layer's scale.",
                        t.px,
                        t.size,
                        self.set.min_text_px,
                        self.h,
                        if severity == Severity::Error { " (less than half of it)" } else { "" }
                    ),
                );
            }
            if self.set.on("outside-title-safe") && layout::on_screen_fraction(&corners, self.w, self.h) > EPS && inside_share(&corners, area, title) < 1.0 - EPS {
                push(
                    "outside-title-safe",
                    Severity::Warning,
                    format!(
                        "{shown} (inside the precomp “{}”) reaches {} past this comp's title-safe area ({}% of the frame per side): move it inward or make it smaller.",
                        self.top.name,
                        overshoot(layout::aabb(&corners), title),
                        self.set.title_safe / 2.0
                    ),
                );
            }
        }
    }
}

fn nc_name(project: &Project, item: ItemId) -> String {
    project.item(item).map(|i| i.name.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------- exposed-edge

/// A pixel with less alpha than this shows what is behind the comp (its background).
const COVER_ALPHA: f32 = 0.5;
/// A frame line (row or column) at an edge counts as empty when at least this share of it is.
const STRIP_SHARE: f64 = 0.5;
/// A frame counts as covered with at most this share of it empty.
const COVERED: f64 = 0.001;
/// An edge strip is only an "exposed edge" when at most this share of the frame is empty (the
/// rest is covered: a full-screen shot that doesn't quite fill the frame).
const MOSTLY_COVERED: f64 = 0.25;
/// A transition gap: at least this share of the frame empty between two covered samples.
const GAP_SHARE: f64 = 0.01;
/// Layer boxes must claim this share of the frame before a sample is rendered for
/// `exposed-edge` (overlay comps that never fill the frame are skipped).
pub const CLAIM_SHARE: f64 = 0.9;

/// How much of a rendered frame is covered.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Coverage {
    /// Share (0–1) of the frame's pixels that show the background.
    pub empty: f64,
    /// Depth (comp pixels) of the empty strip along the left, right, top and bottom edges.
    pub strips: [f64; 4],
}

/// [`Coverage`] of a render (`img`, premultiplied, at `scale` × comp pixels).
pub fn coverage(img: &Image, scale: f64) -> Coverage {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 || scale <= 0.0 {
        return Coverage::default();
    }
    let empty_at = |x: usize, y: usize| img.data.get(y * w + x).is_none_or(|p| p[3] < COVER_ALPHA);
    let n = img.data.iter().filter(|p| p[3] < COVER_ALPHA).count();
    let row = |y: usize| (0..w).filter(|&x| empty_at(x, y)).count() as f64 >= w as f64 * STRIP_SHARE;
    let col = |x: usize| (0..h).filter(|&y| empty_at(x, y)).count() as f64 >= h as f64 * STRIP_SHARE;
    let depth = |lines: &mut dyn Iterator<Item = bool>| lines.take_while(|e| *e).count() as f64 / scale;
    Coverage {
        empty: n as f64 / (w * h) as f64,
        strips: [
            depth(&mut (0..w).map(col)),
            depth(&mut (0..w).rev().map(col)),
            depth(&mut (0..h).map(row)),
            depth(&mut (0..h).rev().map(row)),
        ],
    }
}

/// The share (0–1, capped) of the frame the drawn layers' boxes claim at the context time, and
/// the layer claiming most of it (the shot meant to fill the frame). Whether the frame is
/// certainly covered (an opaque solid or alpha-less footage over all of it) comes third: no
/// render is needed then.
pub fn frame_claim(ctx: &EvalCtx) -> (f64, Option<LayerId>, bool) {
    let comp = ctx.comp;
    let (w, h) = (comp.width as f64, comp.height as f64);
    let frame = [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]];
    let (mut sum, mut best, mut covered) = (0.0, None::<(LayerId, f64)>, false);
    let mattes: Vec<LayerId> = comp.layers.iter().filter_map(|l| l.track_matte.map(|m| m.layer)).collect();
    for l in &comp.layers {
        if mattes.contains(&l.id) || l.switches.adjustment || !has_picture(ctx.project, l) || !layout::visibility(ctx, l).0 {
            continue;
        }
        let Some(g) = layout::layer_geometry(ctx, l).filter(|g| g.projected) else { continue };
        let a = layout::overlap_area(&g.corners, &frame);
        sum += a;
        if best.is_none_or(|(_, b)| a > b) {
            best = Some((l.id, a));
        }
        covered |= a >= w * h * (1.0 - EPS) && opaque_cover(ctx, l);
    }
    let area = (w * h).max(1e-9);
    ((sum / area).min(1.0), best.map(|b| b.0), covered)
}

/// "a 12 px strip along the bottom edge and 3 px along the left edge".
fn strips_text(s: [f64; 4]) -> String {
    let parts: Vec<String> = s
        .iter()
        .zip(["left", "right", "top", "bottom"])
        .filter(|(d, _)| **d >= 1.0)
        .map(|(d, side)| format!("{d:.0} px along the {side} edge"))
        .collect();
    parts.join(" and ")
}

/// `exposed-edge` findings from the coverage of rendered samples (`(time, coverage, the layer
/// meant to fill the frame)`, in time order): an empty strip along a frame edge while the rest
/// is covered, and frames with part of the picture empty between two covered samples (a gap in
/// a transition).
pub fn exposed_edges(comp: &Comp, samples: &[(Tick, Coverage, LayerId)]) -> Vec<(Tick, Finding)> {
    let mut out = vec![];
    let finding = |layer: LayerId, other: Option<LayerId>, message: String| Finding { rule: "exposed-edge", severity: Severity::Warning, layer, other, inner: None, message };
    for (i, (t, c, lid)) in samples.iter().enumerate() {
        let Some(l) = comp.layer(*lid) else { continue };
        if c.strips.iter().any(|d| *d >= 1.0) && c.empty <= MOSTLY_COVERED {
            out.push((
                *t,
                finding(
                    l.id,
                    None,
                    format!(
                        "The frame is not fully covered: a {} shows the comp's background (nothing is drawn there) while {} fills the rest. Scale or move {} so it covers the whole frame, or put a full-frame layer under it.",
                        strips_text(c.strips),
                        name(l),
                        name(l)
                    ),
                ),
            ));
            continue;
        }
        // A gap between two covered samples: reported on the shot going out, with the one
        // coming in as `other`, so one transition is one problem.
        let covered = |s: &&(Tick, Coverage, LayerId)| s.1.empty <= COVERED;
        let before = samples.get(..i).and_then(|b| b.iter().rev().find(covered));
        let after = samples.get(i + 1..).and_then(|a| a.iter().find(covered));
        let (Some(b), Some(a)) = (before, after) else { continue };
        let (Some(out_l), Some(in_l)) = (comp.layer(b.2), comp.layer(a.2)) else { continue };
        if c.empty < GAP_SHARE {
            continue;
        }
        let shots = if out_l.id == in_l.id { name(out_l) } else { format!("{} and {}", name(out_l), name(in_l)) };
        out.push((
            *t,
            finding(
                out_l.id,
                (in_l.id != out_l.id).then_some(in_l.id),
                format!(
                    "{:.0}% of the frame shows the comp's background at {:.3} s, between {:.3} s and {:.3} s where the frame is fully covered: a gap in the transition. Overlap {shots} (or extend one of them) so something covers every frame.",
                    c.empty * 100.0,
                    t.seconds(),
                    b.0.seconds(),
                    a.0.seconds()
                ),
            ),
        ));
    }
    out
}

/// A finding over a span of sample times.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// The worst occurrence (its severity and message).
    pub finding: Finding,
    /// First and last sample time it occurred at.
    pub time: Tick,
    pub until: Tick,
}

/// Merge occurrences of the same (rule, layer, other) across sample times into one problem
/// (`off-beat` and `off-accent` findings stay one per key moment / peak). The worst occurrence's severity and message
/// win. Sorted errors first, then by time, rule and layer.
pub fn merge(occurrences: Vec<(Tick, Finding)>) -> Vec<Problem> {
    let mut out: Vec<Problem> = vec![];
    for (t, f) in occurrences {
        let same = |p: &Problem| {
            p.finding.rule == f.rule
                && p.finding.layer == f.layer
                && p.finding.other == f.other
                && p.finding.inner.as_ref().map(|i| (i.comp, i.layer)) == f.inner.as_ref().map(|i| (i.comp, i.layer))
                && (!matches!(f.rule, "off-beat" | "off-accent") || p.time == t)
        };
        match out.iter_mut().find(|p| same(p)) {
            Some(p) => {
                p.time = p.time.min(t);
                p.until = p.until.max(t);
                if f.severity > p.finding.severity {
                    p.finding = f;
                }
            }
            None => out.push(Problem { finding: f, time: t, until: t }),
        }
    }
    let rule_order = |r: &str| RULES.iter().position(|x| *x == r).unwrap_or(RULES.len());
    out.sort_by(|a, b| {
        b.finding
            .severity
            .cmp(&a.finding.severity)
            .then(a.time.cmp(&b.time))
            .then(rule_order(a.finding.rule).cmp(&rule_order(b.finding.rule)))
            .then(a.finding.layer.0.cmp(&b.finding.layer.0))
    });
    out
}

/// Sample times of a range lint, `start`..=`end` seconds: every `step` seconds, both ends, each
/// layer's in point and every keyframe (in comp time) inside the range, clamped to the comp's
/// first and last frame and de-duplicated. More than `cap` are thinned evenly (and the flag is
/// true).
pub fn sample_times(comp: &Comp, start: f64, end: f64, step: f64, cap: usize) -> (Vec<Tick>, bool) {
    let last = (comp.duration - comp.frame_rate.frame_duration()).max(Tick::ZERO);
    let clamp = |t: Tick| t.clamp(Tick::ZERO, last);
    let inside = |t: f64| t >= start - 1e-9 && t <= end + 1e-9;
    let (t0, t1) = (Tick::from_seconds_f64(start), Tick::from_seconds_f64(end));
    let mut all = vec![clamp(t0), clamp(t1)];
    let dt = Tick::from_seconds_f64(step);
    if dt > Tick::ZERO {
        let mut t = t0 + dt;
        while t < t1 && all.len() < MAX_CANDIDATES {
            all.push(clamp(t));
            t += dt;
        }
    }
    for l in &comp.layers {
        if inside(l.in_point.seconds()) {
            all.push(clamp(l.in_point));
        }
        let mut keys = vec![];
        l.props.walk("", &mut |_, p| keys.extend(p.keys.iter().map(|k| l.comp_time(k.time))));
        all.extend(keys.into_iter().filter(|t| inside(t.seconds())).take(MAX_CANDIDATES).map(clamp));
    }
    all.sort();
    // Times within a microsecond are the same sample.
    let close = Tick::from_seconds_f64(1e-6);
    all.dedup_by(|b, a| *b - *a < close);
    if all.len() <= cap || cap == 0 {
        return (all, false);
    }
    let n = all.len();
    let picked: Vec<Tick> = (0..cap).filter_map(|i| all.get(i * (n - 1) / (cap - 1).max(1)).copied()).collect();
    (picked, true)
}

// ---------------------------------------------------------------- off-beat

/// A moment the eye catches: a transform keyframe or a layer's in point (comp seconds).
#[derive(Clone, Debug, PartialEq)]
pub struct KeyMoment {
    pub layer: LayerId,
    pub time: f64,
    /// "Position keyframe", "in point".
    pub what: String,
}

/// Key moments of `comp` in `start`..=`end` (comp seconds): the keyframes of every layer's
/// Transform properties (Opacity included), and the in points after the comp start of layers
/// with pixels.
pub fn key_moments(project: &Project, comp: &Comp, start: f64, end: f64) -> Vec<KeyMoment> {
    let inside = |t: f64| t >= start - 1e-9 && t <= end + 1e-9;
    let mut out: Vec<KeyMoment> = vec![];
    for l in &comp.layers {
        let mut add = |time: f64, what: String| {
            if inside(time) && !out.iter().any(|m| m.layer == l.id && (m.time - time).abs() < 1e-9) && out.len() < MAX_CANDIDATES {
                out.push(KeyMoment { layer: l.id, time, what });
            }
        };
        if l.in_point > Tick::ZERO && has_picture(project, l) {
            add(l.in_point.seconds(), "in point".into());
        }
        if let Some(tr) = l.transform() {
            tr.walk("", &mut |_, p| {
                for k in &p.keys {
                    add(l.comp_time(k.time).seconds(), format!("{} keyframe", p.name));
                }
            });
        }
    }
    out.sort_by(|a, b| a.time.total_cmp(&b.time));
    out
}

/// `off-beat` findings with their times: key moments within half a beat interval of a beat
/// that miss it by more than one frame. `beats` are sorted comp seconds.
pub fn off_beat(comp: &Comp, moments: &[KeyMoment], beats: &[f64], fps: f64) -> Vec<(Tick, Finding)> {
    let mut out = vec![];
    if beats.len() < 2 || fps.is_nan() || fps <= 0.0 {
        return out;
    }
    let frame = 1.0 / fps;
    let span = beats.last().copied().unwrap_or(0.0) - beats.first().copied().unwrap_or(0.0);
    let mean_interval = span / (beats.len() - 1) as f64;
    for m in moments {
        let i = beats.partition_point(|b| *b < m.time);
        let (prev, next) = (i.checked_sub(1).and_then(|j| beats.get(j)), beats.get(i));
        let (beat, interval) = match (prev, next) {
            (Some(&p), Some(&n)) => (if m.time - p <= n - m.time { p } else { n }, n - p),
            (Some(&p), None) => (p, mean_interval),
            (None, Some(&n)) => (n, mean_interval),
            (None, None) => continue,
        };
        let d = m.time - beat;
        if d.abs() >= interval / 2.0 || d.abs() <= frame + 1e-6 {
            continue;
        }
        let Some(l) = comp.layer(m.layer) else { continue };
        let frames = d.abs() * fps;
        let frames = if (frames - frames.round()).abs() < 0.05 { format!("{:.0}", frames.round()) } else { format!("{frames:.1}") };
        out.push((
            Tick::from_seconds_f64(m.time),
            Finding {
                rule: "off-beat",
                severity: Severity::Warning,
                layer: l.id,
                other: None,
                inner: None,
                message: format!(
                    "{}'s {} at {:.3} s is {frames} frames {} the beat at {:.3} s (beats every {:.3} s, about {:.0} BPM): move it to {:.3} s to land on the beat.",
                    name(l),
                    m.what,
                    m.time,
                    if d > 0.0 { "after" } else { "before" },
                    beat,
                    interval,
                    60.0 / interval.max(1e-9),
                    beat
                ),
            },
        ));
    }
    out
}

/// Onset-envelope hop of the beat tracker (seconds).
const HOP: f64 = 0.005;
/// Tempo range the tracker considers (BPM).
const MIN_BPM: f64 = 50.0;
const MAX_BPM: f64 = 220.0;
/// Smallest energy rise (dB from one 5 ms window to the next) that counts as an onset.
const MIN_ONSET_DB: f64 = 6.0;

/// Beat times (seconds from the start of `mono`) of a mono signal at `rate` Hz.
///
/// Onset strength is the rise of the short-time energy in dB (5 ms windows, 60 dB range). The
/// tempo is the autocorrelation lag of the onset envelope between 50 and 220 BPM with the
/// highest score, weighted towards 120 BPM (a log-normal prior one octave wide). Beats are then
/// placed by dynamic programming (D. Ellis, "Beat Tracking by Dynamic Programming", 2007): each
/// beat on a strong onset, consecutive beats about one period apart. Beats more than half a
/// period before the first or after the last clear onset are dropped. Empty when there are
/// fewer than four clear onsets.
pub fn detect_beats(mono: &[f32], rate: u32) -> Vec<f64> {
    let hop = ((rate as f64 * HOP).round() as usize).max(1);
    let hop_s = hop as f64 / rate.max(1) as f64;
    let n = mono.len() / hop;
    if n < 8 {
        return vec![];
    }
    let db: Vec<f64> = mono.chunks_exact(hop).map(|c| 10.0 * (c.iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / hop as f64 + 1e-12).log10()).collect();
    let top = db.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let floor = top - 60.0;
    let onset: Vec<f64> = std::iter::once(0.0)
        .chain(db.windows(2).map(|w| (w.get(1).copied().unwrap_or(0.0).max(floor) - w.first().copied().unwrap_or(0.0).max(floor)).max(0.0)))
        .collect();
    let peak = onset.iter().copied().fold(0.0, f64::max);
    if peak.is_nan() || peak < MIN_ONSET_DB {
        return vec![];
    }
    // Clear onsets: local maxima of at least 30 % of the strongest (and 6 dB).
    let strong: Vec<usize> = (0..onset.len())
        .filter(|&k| {
            let v = onset.get(k).copied().unwrap_or(0.0);
            v >= (0.3 * peak).max(MIN_ONSET_DB)
                && v >= k.checked_sub(1).and_then(|j| onset.get(j)).copied().unwrap_or(0.0)
                && v > onset.get(k + 1).copied().unwrap_or(0.0)
        })
        .collect();
    let (Some(&first), Some(&last)) = (strong.first(), strong.last()) else { return vec![] };
    if strong.len() < 4 {
        return vec![];
    }
    // Tempo: weighted autocorrelation of the onset envelope, refined between lags.
    let min_lag = (60.0 / MAX_BPM / hop_s).floor().max(2.0) as usize;
    let max_lag = ((60.0 / MIN_BPM / hop_s).ceil() as usize).min(n.saturating_sub(1));
    if max_lag <= min_lag + 1 {
        return vec![];
    }
    let score = |lag: usize| -> f64 {
        let ac: f64 = onset.iter().zip(onset.iter().skip(lag)).map(|(a, b)| a * b).sum();
        let bpm = 60.0 / (lag as f64 * hop_s);
        ac * (-0.5 * (bpm / 120.0).log2().powi(2)).exp()
    };
    let scores: Vec<f64> = (min_lag..=max_lag).map(score).collect();
    let Some((best_i, _)) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else { return vec![] };
    let mut period = (min_lag + best_i) as f64;
    if let (Some(&a), Some(&b), Some(&c)) = (best_i.checked_sub(1).and_then(|i| scores.get(i)), scores.get(best_i), scores.get(best_i + 1)) {
        let den = a - 2.0 * b + c;
        if den.abs() > 1e-12 {
            period += (0.5 * (a - c) / den).clamp(-0.5, 0.5);
        }
    }
    // Dynamic programming: score[k] = onset[k] + best predecessor 0.5–2 periods back, penalised
    // by how far its spacing is from the period (log ratio squared).
    let mean = onset.iter().sum::<f64>() / n as f64;
    let sd = (onset.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt().max(1e-9);
    const TIGHTNESS: f64 = 100.0;
    let (lo, hi) = ((period / 2.0).round().max(1.0) as usize, (period * 2.0).round() as usize);
    let mut total = vec![0.0f64; n];
    let mut back: Vec<Option<usize>> = vec![None; n];
    for k in 0..n {
        let local = onset.get(k).copied().unwrap_or(0.0) / sd;
        let mut best: Option<(f64, usize)> = None;
        for j in k.saturating_sub(hi)..=k.saturating_sub(lo) {
            if k < lo || j >= k {
                break;
            }
            let pen = TIGHTNESS * (((k - j) as f64) / period).ln().powi(2);
            let s = total.get(j).copied().unwrap_or(0.0) - pen;
            if best.is_none_or(|(b, _)| s > b) {
                best = Some((s, j));
            }
        }
        if let (Some(t), Some(bk)) = (total.get_mut(k), back.get_mut(k)) {
            *t = local + best.map_or(0.0, |(s, _)| s.max(0.0));
            *bk = best.filter(|(s, _)| *s > 0.0).map(|(_, j)| j);
        }
    }
    // The best-scoring frame in the last period ends the chain.
    let tail = n.saturating_sub(period.ceil() as usize);
    let Some((mut k, _)) = total.iter().enumerate().skip(tail).max_by(|a, b| a.1.total_cmp(b.1)) else { return vec![] };
    let mut beats = vec![k];
    let mut guard = 0usize;
    while let Some(Some(j)) = back.get(k) {
        if *j >= k || guard > n {
            break;
        }
        beats.push(*j);
        k = *j;
        guard += 1;
    }
    beats.reverse();
    let half = period / 2.0;
    beats.into_iter().filter(|&b| b as f64 >= first as f64 - half && b as f64 <= last as f64 + half).map(|b| b as f64 * hop_s).collect()
}

// ---------------------------------------------------------------- off-accent

/// Samples per keyframe segment when looking for its speed peak (then refined).
const PEAK_SAMPLES: usize = 48;
/// A segment whose speed varies by less than this share of its top speed is flat (linear): no peak.
const FLAT_SHARE: f64 = 0.05;
/// Most speed peaks one lint collects.
pub const MAX_PEAKS: usize = 20_000;
/// Default: frames before the stressed vowel's onset where a peak lands on the accent (Elias's
/// hand-timed peak on "CONtent" in 10-10 MAIN: the k burst + 2 frames = the vowel − 1 frame).
pub const DEFAULT_ACCENT_LEAD: f64 = 1.0;
/// Default: a peak this many frames or fewer from an accent (but more than one off) is a near miss.
pub const DEFAULT_ACCENT_WINDOW: f64 = 4.0;

/// A speed-graph peak: where a keyframed property moves fastest between two keyframes.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeedPeak {
    pub layer: LayerId,
    /// The property's uid path inside the layer (`@3/@7`).
    pub path: String,
    /// The property's name ("Scale", "Position").
    pub property: String,
    /// Comp seconds.
    pub time: f64,
    /// Top speed (property units per second; the length of the velocity for several dimensions).
    pub speed: f64,
    /// The comp `layer` is in when it sits in a precomp ([`speed_peaks_deep`]); `None`: the
    /// comp searched.
    pub comp: Option<ItemId>,
    /// The precomp layers from the comp searched down to `comp`, outermost first (empty when
    /// `comp` is `None`). `time` is mapped through them.
    pub via: Vec<Hop>,
}

/// Where a peak lands on a stressed syllable: `lead` frames before its vowel, never before its
/// onset (comp seconds).
pub fn accent_time(s: &effectcraft_project::speech::Syllable, fps: f64, lead: f64) -> f64 {
    let (t, v) = (s.t.seconds(), s.vowel.seconds());
    if fps.is_finite() && fps > 0.0 { (v - lead / fps).max(t) } else { t }
}

/// The speed-graph peak of the keyframe segment `i` → `i + 1` of `keys`: (layer seconds, top
/// speed), refined to well under a millisecond. `None` for a hold, equal values, keys out of
/// order or a linear (flat-speed) segment. [`speed_peaks`] and `keys.alignPeak` both use it.
pub fn segment_peak(keys: &[effectcraft_keyframe::Keyframe], i: usize, spatial: bool) -> Option<(f64, f64)> {
    let (a, b) = (keys.get(i)?, keys.get(i.checked_add(1)?)?);
    if a.out_interp == effectcraft_keyframe::Interp::Hold || a.value == b.value || b.time <= a.time {
        return None;
    }
    let speed = |lt: f64| {
        let v = effectcraft_keyframe::speed(keys, Tick::from_seconds_f64(lt), spatial);
        if v.is_finite() { v } else { 0.0 }
    };
    let (t0, t1) = (a.time.seconds(), b.time.seconds());
    let dt = (t1 - t0) / PEAK_SAMPLES as f64;
    let at = |i: f64| t0 + (i + 0.5) * dt;
    let (mut best, mut top, mut low) = (0usize, f64::MIN, f64::MAX);
    for i in 0..PEAK_SAMPLES {
        let v = speed(at(i as f64));
        if v > top {
            (best, top) = (i, v);
        }
        low = low.min(v);
    }
    if top <= 1e-9 || top - low < FLAT_SHARE * top {
        return None;
    }
    // Golden-section refine around the best sample (the speed is unimodal there).
    let (mut lo, mut hi) = (at(best as f64 - 1.0).max(t0), at(best as f64 + 1.0).min(t1));
    for _ in 0..24 {
        let m1 = hi - (hi - lo) * 0.618_034;
        let m2 = lo + (hi - lo) * 0.618_034;
        if speed(m1) < speed(m2) { lo = m1 } else { hi = m2 }
    }
    let lt = (lo + hi) / 2.0;
    Some((lt, speed(lt).max(top)))
}

/// Speed-graph peaks of every keyframed property of `comp`'s layers in `start..=end` (comp
/// seconds): for each pair of neighbouring keyframes with different values and no hold between
/// them, the time of the highest speed (keyframes only; expressions are not evaluated), when it
/// falls inside the layer's in/out. Linear segments (constant speed) have no peak. Sorted by time.
pub fn speed_peaks(comp: &Comp, start: f64, end: f64) -> Vec<SpeedPeak> {
    let mut out: Vec<SpeedPeak> = vec![];
    for l in &comp.layers {
        l.props.walk("", &mut |path, p| {
            if p.keys.len() < 2 || out.len() >= MAX_PEAKS {
                return;
            }
            for (i, pair) in p.keys.windows(2).enumerate() {
                let [a, b] = pair else { continue };
                let (a_t, b_t) = (l.comp_time(a.time).seconds(), l.comp_time(b.time).seconds());
                if a_t.max(b_t) < start - 1e-9 || a_t.min(b_t) > end + 1e-9 {
                    continue;
                }
                let Some((lt, top)) = segment_peak(&p.keys, i, p.spatial) else { continue };
                let time = l.comp_time(Tick::from_seconds_f64(lt)).seconds();
                // Outside the layer's in/out nothing shows, so the peak can't be seen.
                let shown = time >= l.in_point.seconds() - 1e-9 && time <= l.out_point.seconds() + 1e-9;
                if !shown || time < start - 1e-9 || time > end + 1e-9 {
                    continue;
                }
                if out.iter().any(|q| q.layer == l.id && q.path == path && (q.time - time).abs() < 1e-6) {
                    continue;
                }
                out.push(SpeedPeak { layer: l.id, path: path.to_string(), property: p.name.clone(), time, speed: top, comp: None, via: vec![] });
            }
        });
    }
    out.sort_by(|a, b| a.time.total_cmp(&b.time));
    out
}

/// Most precomp layers [`speed_peaks_deep`] looks into.
const MAX_PRECOMP_VISITS: usize = 4096;

/// Speed peaks of a comp and of everything nested in it ([`speed_peaks_deep`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeepPeaks {
    pub peaks: Vec<SpeedPeak>,
    /// Why some precomps were not searched (time-remapped precomp layers).
    pub skipped: Vec<String>,
}

/// [`speed_peaks`] of `cid`'s own layers and of the layers in its precomps, at most
/// [`precomp_time::MAX_DEPTH`] deep, each peak's time mapped up through every precomp layer's
/// start and stretch to `cid`'s seconds. A nested peak is kept only when every precomp layer on
/// the way shows it (inside its in/out, and inside its comp's duration). Time-remapped precomp
/// layers are not searched (see `skipped`). Sorted by time.
pub fn speed_peaks_deep(project: &Project, cid: ItemId, start: f64, end: f64) -> DeepPeaks {
    let Some(comp) = project.comp(cid) else { return DeepPeaks::default() };
    let mut out = DeepPeaks { peaks: speed_peaks(comp, start, end), skipped: vec![] };
    let mut budget = MAX_PRECOMP_VISITS;
    nested_peaks(project, (cid, comp), (start, end), &mut vec![], &mut vec![cid], &mut out, &mut budget);
    out.peaks.sort_by(|a, b| a.time.total_cmp(&b.time));
    out.peaks.truncate(MAX_PEAKS);
    out
}

/// Peaks inside the precomp layers of `at` whose shown part overlaps `lo..=hi` (`at`'s seconds).
fn nested_peaks(
    project: &Project,
    at: (ItemId, &Comp),
    (lo, hi): (f64, f64),
    chain: &mut Vec<Hop>,
    comps: &mut Vec<ItemId>,
    out: &mut DeepPeaks,
    budget: &mut usize,
) {
    if chain.len() >= precomp_time::MAX_DEPTH {
        return;
    }
    for l in &at.1.layers {
        let LayerSource::Comp { item } = l.source else { continue };
        if comps.contains(&item) || out.peaks.len() >= MAX_PEAKS || *budget == 0 {
            continue;
        }
        let Some(inner) = project.comp(item) else { continue };
        // The part of the window this precomp layer shows.
        let (a, b) = (lo.max(l.in_point.seconds()), hi.min(l.out_point.seconds()));
        if a > b + 1e-9 {
            continue;
        }
        let h = match precomp_time::hop(at.0, l) {
            Ok(h) => h,
            Err(why) => {
                if !out.skipped.contains(&why) {
                    out.skipped.push(why);
                }
                continue;
            }
        };
        *budget = budget.saturating_sub(1);
        let one = [h];
        let (x, y) = (precomp_time::down(&one, a), precomp_time::down(&one, b));
        let (x, y) = (x.min(y).max(0.0), x.max(y).min(inner.duration.seconds()));
        if x.is_nan() || y.is_nan() || x > y + 1e-9 {
            continue;
        }
        chain.push(h);
        comps.push(item);
        for mut pk in speed_peaks(inner, x, y) {
            pk.time = precomp_time::up(chain, pk.time);
            (pk.comp, pk.via) = (Some(item), chain.clone());
            out.peaks.push(pk);
        }
        nested_peaks(project, (item, inner), (x, y), chain, comps, out, budget);
        chain.pop();
        comps.pop();
    }
}

/// A peak matched to the nearest accent (a stressed syllable's [`accent_time`]).
#[derive(Clone, Debug, PartialEq)]
pub struct AccentMatch<'a> {
    pub peak: SpeedPeak,
    pub word: &'a effectcraft_project::speech::SpeechWord,
    pub syllable: effectcraft_project::speech::Syllable,
    /// The accent time (comp seconds).
    pub target: f64,
    /// Peak − target, in frames (negative: the peak is early).
    pub offset_frames: f64,
}

/// Each peak with its nearest accent in `map` (`None` without stressed syllables).
pub fn match_accents<'a>(peaks: Vec<SpeedPeak>, map: &'a effectcraft_project::speech::SpeechMap, fps: f64, lead: f64) -> Vec<AccentMatch<'a>> {
    let stressed = map.stressed();
    let targets: Vec<f64> = stressed.iter().map(|(_, s)| accent_time(s, fps, lead)).collect();
    let fps = if fps.is_finite() && fps > 0.0 { fps } else { 30.0 };
    peaks
        .into_iter()
        .filter_map(|peak| {
            let i = targets.partition_point(|t| *t < peak.time);
            let near = [i.checked_sub(1), Some(i)]
                .into_iter()
                .flatten()
                .filter_map(|j| Some((j, *targets.get(j)?)))
                .min_by(|a, b| (a.1 - peak.time).abs().total_cmp(&(b.1 - peak.time).abs()))?;
            let (word, syllable) = *stressed.get(near.0)?;
            Some(AccentMatch { offset_frames: (peak.time - near.1) * fps, target: near.1, word, syllable: *syllable, peak })
        })
        .collect()
}

/// `off-accent` findings with their times: speed-graph peaks ([`speed_peaks_deep`] of `cid`, so
/// moves inside precomps count) within `window` frames of an accent (a stressed syllable's onset,
/// `lead` frames before its vowel) that miss it by more than one frame. A nested peak's finding is
/// on the precomp layer in `cid`; its message names the nested layer and the `keys.alignPeak`
/// call that fixes it. Also returns why some precomps were not searched.
pub fn off_accent(
    project: &Project,
    cid: ItemId,
    map: &effectcraft_project::speech::SpeechMap,
    start: f64,
    end: f64,
    fps: f64,
    lead: f64,
    window: f64,
) -> (Vec<(Tick, Finding)>, Vec<String>) {
    let mut out = vec![];
    let Some(comp) = project.comp(cid) else { return (out, vec![]) };
    if fps.is_nan() || fps <= 0.0 {
        return (out, vec![]);
    }
    let deep = speed_peaks_deep(project, cid, start, end);
    for m in match_accents(deep.peaks, map, fps, lead) {
        let d = m.offset_frames;
        if d.abs() <= 1.0 + 1e-6 || d.abs() > window + 1e-6 {
            continue;
        }
        // The layer in `cid` the finding is on: the peak's own, or the outermost precomp layer.
        let top = m.peak.via.first().map_or(m.peak.layer, |h| h.layer);
        let Some(shown) = comp.layer(top) else { continue };
        let inner = m.peak.comp.and_then(|c| project.comp(c));
        let Some(l) = inner.map_or(Some(shown), |c| c.layer(m.peak.layer)) else { continue };
        let frames = if (d.abs() - d.abs().round()).abs() < 0.05 { format!("{:.0}", d.abs().round()) } else { format!("{:.1}", d.abs()) };
        let (where_, call) = match m.peak.comp {
            None => (String::new(), format!("layer: {}, path: \"{}\"", l.id.0, m.peak.path)),
            Some(c) => {
                let names: Vec<String> = m.peak.via.iter().map(|h| project.comp(h.comp).and_then(|x| x.layer(h.layer)).map(name).unwrap_or_default()).collect();
                let via: Vec<String> = m.peak.via.iter().map(|h| h.layer.0.to_string()).collect();
                (
                    format!(" (in precomp {}, via {})", project.item(c).map(|i| format!("\"{}\"", i.name)).unwrap_or_default(), names.join(" → ")),
                    format!("comp: {}, layer: {}, path: \"{}\", targetComp: {}, via: [{}]", c.0, l.id.0, m.peak.path, cid.0, via.join(", ")),
                )
            }
        };
        out.push((
            Tick::from_seconds_f64(m.peak.time),
            Finding {
                rule: "off-accent",
                severity: Severity::Warning,
                layer: shown.id,
                other: None,
                inner: None,
                message: format!(
                    "{}'s {} speed peak{where_} at {:.3} s (frame {:.1}) is {frames} frames {} the stressed syllable of \"{}\" (onset {:.3} s, vowel {:.3} s; accent at {:.3} s, frame {:.0}): move the peak by {:+.0} frames, to {:.3} s, to land on the accent (keys.alignPeak {{{call}, frame: {:.0}}}).",
                    name(l),
                    m.peak.property,
                    m.peak.time,
                    m.peak.time * fps,
                    if d > 0.0 { "after" } else { "before" },
                    m.word.word,
                    m.syllable.t.seconds(),
                    m.syllable.vowel.seconds(),
                    m.target,
                    (m.target * fps).round(),
                    -d.round(),
                    m.target,
                    (m.target * fps).round()
                ),
            },
        ));
    }
    (out, deep.skipped)
}
