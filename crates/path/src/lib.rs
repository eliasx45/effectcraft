//! Bezier shapes for shape layers, masks and text.
//!
//! Geometry is kept in [`kurbo::BezPath`]; anti-aliased coverage comes from tiny-skia's scanline
//! rasteriser (BSD-3) and is converted to our `f32` [`Mask`]. Path operations (trim, round corners,
//! pucker & bloat, zig zag, twist, wiggle, offset, merge) live in [`ops`]; curve-preserving booleans
//! (for Merge Paths) in [`boolean`] and curve offsetting in [`offset`].

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod boolean;
pub mod feather;
mod fit;
pub mod interp;
pub mod offset;
pub mod ops;
pub mod trace;
pub mod varstroke;

use effectcraft_geom::Mat3;
use effectcraft_keyframe::ShapePath;
use effectcraft_raster::Mask;
pub use kurbo::{BezPath, PathEl, Point};
use kurbo::{Shape, Vec2 as KVec2};

/// Convert the AE vertex/tangent model to a kurbo path.
pub fn to_kurbo(p: &ShapePath) -> BezPath {
    let mut out = BezPath::new();
    let n = p.vertices.len();
    if n == 0 {
        return out;
    }
    let v = |i: usize| Point::new(p.vertices[i][0], p.vertices[i][1]);
    let ot = |i: usize| p.out_tangents.get(i).copied().unwrap_or([0.0; 2]);
    let it = |i: usize| p.in_tangents.get(i).copied().unwrap_or([0.0; 2]);
    out.move_to(v(0));
    let seg = |out: &mut BezPath, a: usize, b: usize| {
        let (o, i) = (ot(a), it(b));
        if o == [0.0; 2] && i == [0.0; 2] {
            out.line_to(v(b));
        } else {
            out.curve_to(v(a) + KVec2::new(o[0], o[1]), v(b) + KVec2::new(i[0], i[1]), v(b));
        }
    };
    for i in 0..n - 1 {
        seg(&mut out, i, i + 1);
    }
    if p.closed && n > 1 {
        seg(&mut out, n - 1, 0);
        out.close_path();
    }
    out
}

/// Convert a kurbo path (first subpath) back to the vertex model (quads are elevated).
pub fn from_kurbo(path: &BezPath) -> Vec<ShapePath> {
    let mut out = Vec::new();
    let mut cur: Option<ShapePath> = None;
    let mut last = Point::ZERO;
    let flush = |cur: &mut Option<ShapePath>, out: &mut Vec<ShapePath>| {
        if let Some(mut c) = cur.take() {
            // A closing segment ending on the first vertex duplicates it.
            if c.closed && c.vertices.len() > 1 {
                let f = c.vertices[0];
                let l = *c.vertices.last().unwrap_or(&f);
                if (f[0] - l[0]).abs() < 1e-9 && (f[1] - l[1]).abs() < 1e-9 {
                    let it = c.in_tangents.pop().unwrap_or([0.0; 2]);
                    c.vertices.pop();
                    c.out_tangents.pop();
                    c.in_tangents[0] = it;
                }
            }
            if !c.vertices.is_empty() {
                out.push(c);
            }
        }
    };
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                flush(&mut cur, &mut out);
                cur = Some(ShapePath {
                    vertices: vec![[p.x, p.y]],
                    in_tangents: vec![[0.0; 2]],
                    out_tangents: vec![[0.0; 2]],
                    closed: false,
                    feather: Vec::new(),
                });
                last = p;
            }
            PathEl::LineTo(p) => {
                if let Some(c) = cur.as_mut() {
                    c.vertices.push([p.x, p.y]);
                    c.in_tangents.push([0.0; 2]);
                    c.out_tangents.push([0.0; 2]);
                }
                last = p;
            }
            PathEl::QuadTo(q, p) => {
                let c1 = last + (q - last) * (2.0 / 3.0);
                let c2 = p + (q - p) * (2.0 / 3.0);
                if let Some(c) = cur.as_mut() {
                    if let Some(o) = c.out_tangents.last_mut() {
                        *o = [c1.x - last.x, c1.y - last.y];
                    }
                    c.vertices.push([p.x, p.y]);
                    c.in_tangents.push([c2.x - p.x, c2.y - p.y]);
                    c.out_tangents.push([0.0; 2]);
                }
                last = p;
            }
            PathEl::CurveTo(c1, c2, p) => {
                if let Some(c) = cur.as_mut() {
                    if let Some(o) = c.out_tangents.last_mut() {
                        *o = [c1.x - last.x, c1.y - last.y];
                    }
                    c.vertices.push([p.x, p.y]);
                    c.in_tangents.push([c2.x - p.x, c2.y - p.y]);
                    c.out_tangents.push([0.0; 2]);
                }
                last = p;
            }
            PathEl::ClosePath => {
                if let Some(c) = cur.as_mut() {
                    c.closed = true;
                }
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Rectangle path centred at `pos` with corner `roundness` (AE draws clockwise from the top-right).
pub fn rect(size: [f64; 2], pos: [f64; 2], roundness: f64) -> BezPath {
    let r = kurbo::Rect::from_center_size(Point::new(pos[0], pos[1]), (size[0].abs(), size[1].abs()));
    let rad = roundness.clamp(0.0, size[0].abs().min(size[1].abs()) / 2.0);
    if rad > 0.0 { r.to_rounded_rect(rad).to_path(0.1) } else { r.to_path(0.1) }
}

pub fn ellipse(size: [f64; 2], pos: [f64; 2]) -> BezPath {
    to_kurbo(&ShapePath::ellipse(pos, size[0], size[1]))
}

/// Polystar: a star (`inner` radius used) or polygon, rotation in degrees, roundness in percent.
pub fn polystar(star: bool, points: f64, pos: [f64; 2], rotation: f64, inner: f64, outer: f64, inner_round: f64, outer_round: f64) -> BezPath {
    let n = points.max(3.0).round() as usize;
    let verts = if star { n * 2 } else { n };
    let mut path = BezPath::new();
    let step = std::f64::consts::TAU / verts as f64;
    let start = (rotation - 90.0).to_radians();
    let mut pts = Vec::with_capacity(verts);
    for i in 0..verts {
        let r = if star && i % 2 == 1 { inner } else { outer };
        let a = start + step * i as f64;
        pts.push((Point::new(pos[0] + r * a.cos(), pos[1] + r * a.sin()), a, r, if star && i % 2 == 1 { inner_round } else { outer_round }));
    }
    let tangent = |(p, a, r, round): (Point, f64, f64, f64), sign: f64| -> Point {
        if round == 0.0 {
            return p;
        }
        // Tangent perpendicular to the radius, length proportional to the arc between vertices.
        let len = r * step * 0.25 * round / 100.0 * 4.0 / 3.0 * 0.75;
        let d = KVec2::new(-a.sin(), a.cos()) * len * sign;
        p + d
    };
    path.move_to(pts[0].0);
    for i in 0..verts {
        let a = pts[i];
        let b = pts[(i + 1) % verts];
        if a.3 == 0.0 && b.3 == 0.0 {
            path.line_to(b.0);
        } else {
            path.curve_to(tangent(a, 1.0), tangent(b, -1.0), b.0);
        }
    }
    path.close_path();
    path
}

fn to_skia(path: &BezPath) -> Option<tiny_skia::Path> {
    let mut pb = tiny_skia::PathBuilder::new();
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => pb.move_to(p.x as f32, p.y as f32),
            PathEl::LineTo(p) => pb.line_to(p.x as f32, p.y as f32),
            PathEl::QuadTo(a, p) => pb.quad_to(a.x as f32, a.y as f32, p.x as f32, p.y as f32),
            PathEl::CurveTo(a, b, p) => pb.cubic_to(a.x as f32, a.y as f32, b.x as f32, b.y as f32, p.x as f32, p.y as f32),
            PathEl::ClosePath => pb.close(),
        }
    }
    pb.finish()
}

fn from_skia(p: &tiny_skia::Path) -> BezPath {
    let mut out = BezPath::new();
    let pt = |p: tiny_skia::Point| Point::new(p.x as f64, p.y as f64);
    for seg in p.segments() {
        match seg {
            tiny_skia::PathSegment::MoveTo(a) => out.move_to(pt(a)),
            tiny_skia::PathSegment::LineTo(a) => out.line_to(pt(a)),
            tiny_skia::PathSegment::QuadTo(a, b) => out.quad_to(pt(a), pt(b)),
            tiny_skia::PathSegment::CubicTo(a, b, c) => out.curve_to(pt(a), pt(b), pt(c)),
            tiny_skia::PathSegment::Close => out.close_path(),
        }
    }
    out
}

fn skia_transform(m: &Mat3) -> tiny_skia::Transform {
    let a = &m.0;
    tiny_skia::Transform::from_row(a[0][0] as f32, a[1][0] as f32, a[0][1] as f32, a[1][1] as f32, a[0][2] as f32, a[1][2] as f32)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// How far past the mask device coordinates may reach before a path is clipped first: tiny-skia's
/// edges are 16.16 fixed point and anti-aliasing scales them by 4, so far-away points (a huge
/// scale, an SVG with coordinates of 1e30) overflow and trip its assertions.
const SAFE_REACH: f64 = 4096.0;

enum Extent {
    /// Every point is near the mask: rasterise as is.
    Safe,
    /// Some point is far away: clip to the mask first.
    Huge,
    /// Not finite: nothing to draw.
    Unusable,
}

fn device_extent(path: &BezPath, m: kurbo::Affine, w: u32, h: u32) -> Extent {
    let (w, h) = (f64::from(w), f64::from(h));
    let mut huge = false;
    for el in path.elements() {
        let pts: &[Point] = match el {
            PathEl::MoveTo(p) | PathEl::LineTo(p) => std::slice::from_ref(p),
            PathEl::QuadTo(a, p) => &[*a, *p],
            PathEl::CurveTo(a, b, p) => &[*a, *b, *p],
            PathEl::ClosePath => &[],
        };
        for p in pts {
            let d = m * *p;
            if !(d.x.is_finite() && d.y.is_finite()) {
                return Extent::Unusable;
            }
            huge |= d.x < -SAFE_REACH || d.y < -SAFE_REACH || d.x > w + SAFE_REACH || d.y > h + SAFE_REACH;
        }
    }
    if huge { Extent::Huge } else { Extent::Safe }
}

/// `path` (device space) flattened and clipped to a little more than the `w × h` mask, one closed
/// polygon per subpath (Sutherland–Hodgman against a rectangle keeps every winding number inside
/// it, so both fill rules give the same coverage there).
fn clip_to_rect(path: &BezPath, w: u32, h: u32) -> BezPath {
    let (x0, y0, x1, y1) = (-2.0, -2.0, f64::from(w) + 2.0, f64::from(h) + 2.0);
    let mut polys: Vec<Vec<Point>> = vec![];
    kurbo::flatten(path, 0.05, |el| match el {
        PathEl::MoveTo(p) => polys.push(vec![p]),
        PathEl::LineTo(p) => match polys.last_mut() {
            Some(poly) => poly.push(p),
            None => polys.push(vec![p]),
        },
        _ => {}
    });
    let mut out = BezPath::new();
    for poly in polys {
        let mut pts = poly;
        // Keep the inside of each edge in turn: left, right, top, bottom.
        for edge in [Edge::Left(x0), Edge::Right(x1), Edge::Top(y0), Edge::Bottom(y1)] {
            let Some(&last) = pts.last() else { break };
            let mut next = Vec::with_capacity(pts.len() + 2);
            let mut a = last;
            for &b in &pts {
                match (edge.inside(a), edge.inside(b)) {
                    (true, true) => next.push(b),
                    (true, false) => next.push(edge.cross(a, b)),
                    (false, true) => {
                        next.push(edge.cross(a, b));
                        next.push(b);
                    }
                    (false, false) => {}
                }
                a = b;
            }
            pts = next;
        }
        let mut it = pts.into_iter();
        if let Some(first) = it.next() {
            out.move_to(first);
            for p in it {
                out.line_to(p);
            }
            out.close_path();
        }
    }
    out
}

/// One side of the clip rectangle.
#[derive(Clone, Copy)]
enum Edge {
    Left(f64),
    Right(f64),
    Top(f64),
    Bottom(f64),
}

impl Edge {
    fn inside(self, p: Point) -> bool {
        match self {
            Edge::Left(x) => p.x >= x,
            Edge::Right(x) => p.x <= x,
            Edge::Top(y) => p.y >= y,
            Edge::Bottom(y) => p.y <= y,
        }
    }

    /// Where segment `a`–`b` (one end on each side) crosses this edge.
    fn cross(self, a: Point, b: Point) -> Point {
        match self {
            Edge::Left(x) | Edge::Right(x) => {
                let t = if b.x == a.x { 0.0 } else { (x - a.x) / (b.x - a.x) };
                Point::new(x, a.y + (b.y - a.y) * t)
            }
            Edge::Top(y) | Edge::Bottom(y) => {
                let t = if b.y == a.y { 0.0 } else { (y - a.y) / (b.y - a.y) };
                Point::new(a.x + (b.x - a.x) * t, y)
            }
        }
    }
}

/// Anti-aliased coverage of `paths` (in local space) transformed by `m` into a `w × h` mask.
pub fn fill_coverage(paths: &[BezPath], m: &Mat3, w: u32, h: u32, rule: FillRule) -> Mask {
    let mut out = Mask::new(w, h, 0.0);
    let Some(mut mask) = tiny_skia::Mask::new(w.max(1), h.max(1)) else { return out };
    let mut all = BezPath::new();
    for p in paths {
        all.extend(p.elements().iter().copied());
    }
    let fr = match rule {
        FillRule::NonZero => tiny_skia::FillRule::Winding,
        FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
    };
    let a = &m.0;
    let to_device = kurbo::Affine::new([a[0][0], a[1][0], a[0][1], a[1][1], a[0][2], a[1][2]]);
    match device_extent(&all, to_device, w, h) {
        Extent::Unusable => return out,
        Extent::Safe => {
            let Some(sp) = to_skia(&all) else { return out };
            mask.fill_path(&sp, fr, true, skia_transform(m));
        }
        Extent::Huge => {
            // Clip in device space first: the result inside the mask is the same.
            let clipped = clip_to_rect(&(to_device * all), w, h);
            let Some(sp) = to_skia(&clipped) else { return out };
            mask.fill_path(&sp, fr, true, tiny_skia::Transform::identity());
        }
    }
    for (o, &v) in out.data.iter_mut().zip(mask.data()) {
        *o = v as f32 / 255.0;
    }
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cap {
    #[default]
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Join {
    #[default]
    Miter,
    Round,
    Bevel,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    pub width: f64,
    pub cap: Cap,
    pub join: Join,
    pub miter: f64,
    /// Dash pattern (dash, gap, …) and offset.
    pub dash: Option<(Vec<f64>, f64)>,
    /// Stroke Taper (variable width at the ends).
    pub taper: Option<varstroke::Taper>,
    /// Stroke Wave (periodic width).
    pub wave: Option<varstroke::Wave>,
}

impl StrokeStyle {
    /// Whether the width varies along the path (Taper or Wave active).
    pub fn is_variable(&self) -> bool {
        self.taper.as_ref().is_some_and(|t| t.is_active()) || self.wave.as_ref().is_some_and(|w| w.is_active())
    }
}

impl Default for StrokeStyle {
    fn default() -> Self {
        StrokeStyle { width: 1.0, cap: Cap::Butt, join: Join::Miter, miter: 4.0, dash: None, taper: None, wave: None }
    }
}

/// Outline of a stroke as a fillable path (local space). `res_scale` = device/local scale.
pub fn stroke_outline(paths: &[BezPath], style: &StrokeStyle, res_scale: f64) -> Option<BezPath> {
    if style.width <= 0.0 {
        return None;
    }
    if style.is_variable() {
        return varstroke::outline(paths, style, res_scale);
    }
    let mut all = BezPath::new();
    for p in paths {
        all.extend(p.elements().iter().copied());
    }
    let mut sp = to_skia(&all)?;
    if let Some((d, off)) = &style.dash {
        let arr: Vec<f32> = d.iter().map(|v| v.max(0.0) as f32).collect();
        if arr.len() >= 2 && arr.iter().any(|v| *v > 0.0) {
            let arr = if arr.len() % 2 == 1 { [arr.clone(), arr].concat() } else { arr };
            if let Some(dash) = tiny_skia::StrokeDash::new(arr, *off as f32) {
                sp = sp.dash(&dash, res_scale as f32)?;
            }
        }
    }
    let st = tiny_skia::Stroke {
        width: style.width as f32,
        miter_limit: style.miter.max(1.0) as f32,
        line_cap: match style.cap {
            Cap::Butt => tiny_skia::LineCap::Butt,
            Cap::Round => tiny_skia::LineCap::Round,
            Cap::Square => tiny_skia::LineCap::Square,
        },
        line_join: match style.join {
            Join::Miter => tiny_skia::LineJoin::Miter,
            Join::Round => tiny_skia::LineJoin::Round,
            Join::Bevel => tiny_skia::LineJoin::Bevel,
        },
        dash: None,
    };
    let out = sp.stroke(&st, res_scale.max(0.01) as f32)?;
    Some(from_skia(&out))
}

/// Coverage of a stroke. A variable-width stroke (Taper, Wave) is rasterised finely
/// ([`fill_coverage_fine`]): its edges run almost parallel to the path, where tiny-skia's four
/// sub-scanlines per pixel turned a thin taper into a band of constant thickness fading in
/// quarter steps (VW-TAPER).
pub fn stroke_coverage(paths: &[BezPath], style: &StrokeStyle, m: &Mat3, w: u32, h: u32) -> Mask {
    match stroke_outline(paths, style, m.mean_scale()) {
        Some(o) if style.is_variable() => fill_coverage_fine(&[o], m, w, h, FillRule::NonZero),
        Some(o) => fill_coverage(&[o], m, w, h, FillRule::NonZero),
        None => Mask::new(w, h, 0.0),
    }
}

/// The most supersampled pixels [`fill_coverage_fine`] rasterises (64 MiB of `f32`); larger
/// masks are supersampled less, down to plain [`fill_coverage`].
const FINE_MAX_PIXELS: u64 = 16 << 20;

/// [`fill_coverage`] rasterised at up to 4 × 4 the resolution and box-averaged down: coverage in
/// steps of 1/16 of a pixel row instead of tiny-skia's 1/4 (its anti-aliasing takes four
/// sub-scanlines per pixel), so a nearly horizontal or vertical edge that moves less than a
/// pixel over many pixels — a thin tapered stroke — shades smoothly instead of in plateaus.
pub fn fill_coverage_fine(paths: &[BezPath], m: &Mat3, w: u32, h: u32, rule: FillRule) -> Mask {
    let px = u64::from(w) * u64::from(h);
    let k = [4u32, 2].into_iter().find(|k| px.saturating_mul(u64::from(k * k)) <= FINE_MAX_PIXELS).unwrap_or(1);
    let (Some(kw), Some(kh)) = (w.checked_mul(k), h.checked_mul(k)) else { return fill_coverage(paths, m, w, h, rule) };
    if k == 1 {
        return fill_coverage(paths, m, w, h, rule);
    }
    let kf = f64::from(k);
    let big = fill_coverage(paths, &(Mat3::scale(effectcraft_geom::vec2(kf, kf)) * *m), kw, kh, rule);
    let mut out = Mask::new(w, h, 0.0);
    let (k, kw) = (k as usize, kw as usize);
    let inv = 1.0 / (k * k) as f32;
    for (y, row) in out.data.chunks_mut(w.max(1) as usize).enumerate() {
        for (x, o) in row.iter_mut().enumerate() {
            let mut sum = 0.0f32;
            for sy in 0..k {
                let start = (y * k + sy) * kw + x * k;
                sum += big.data.get(start..start + k).map_or(0.0, |s| s.iter().sum());
            }
            *o = sum * inv;
        }
    }
    out
}

/// Bounding box of paths in local space.
pub fn bounds(paths: &[BezPath]) -> Option<kurbo::Rect> {
    let mut r: Option<kurbo::Rect> = None;
    for p in paths {
        if p.elements().is_empty() {
            continue;
        }
        let b = p.bounding_box();
        r = Some(r.map_or(b, |a| a.union(b)));
    }
    r
}

/// Transform paths by an affine Mat3.
pub fn transform(paths: &[BezPath], m: &Mat3) -> Vec<BezPath> {
    let a = &m.0;
    let k = kurbo::Affine::new([a[0][0], a[1][0], a[0][1], a[1][1], a[0][2], a[1][2]]);
    paths.iter().map(|p| k * p.clone()).collect()
}

#[cfg(test)]
mod tests {
    /// Fuzzing found an SVG whose huge coordinates overflowed tiny-skia's fixed-point edges and
    /// tripped its assertions. Far-away paths are now clipped to the mask first, with the same
    /// coverage inside it.
    #[test]
    fn huge_coordinates_are_clipped_not_overflowed() {
        use kurbo::Shape as _;
        let id = Mat3::default();
        // A rectangle reaching 1e9 px past a 64 × 64 mask covers it all, like a mask-sized one.
        let huge = kurbo::Rect::new(-1e9, -1e9, 1e9, 1e9).to_path(0.1);
        let small = kurbo::Rect::new(-10.0, -10.0, 74.0, 74.0).to_path(0.1);
        for rule in [FillRule::NonZero, FillRule::EvenOdd] {
            let a = fill_coverage(std::slice::from_ref(&huge), &id, 64, 64, rule);
            let b = fill_coverage(std::slice::from_ref(&small), &id, 64, 64, rule);
            assert_eq!(a.data, b.data);
        }
        // A triangle with one far corner: the same coverage inside the mask as drawn without
        // clipping where tiny-skia copes (a corner 4000 px away), within anti-aliasing rounding.
        let tri = |far: f64| {
            let mut p = BezPath::new();
            p.move_to((8.0, 8.0));
            p.line_to((56.0, 8.0));
            p.line_to((8.0 + far, 8.0 + far));
            p.close_path();
            p
        };
        let near = fill_coverage(&[tri(3000.0)], &id, 64, 64, FillRule::NonZero);
        let clipped = clip_to_rect(&tri(3000.0), 64, 64);
        let via_clip = fill_coverage(&[clipped], &id, 64, 64, FillRule::NonZero);
        let worst = near.data.iter().zip(&via_clip.data).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.02, "{worst}");
        // Scaled by 1e12, non-finite: no panic.
        let mut big = Mat3::default();
        big.0[0][0] = 1e12;
        big.0[1][1] = 1e12;
        let _ = fill_coverage(&[tri(30.0)], &big, 64, 64, FillRule::EvenOdd);
        let mut nan = Mat3::default();
        nan.0[0][2] = f64::NAN;
        assert!(fill_coverage(&[tri(30.0)], &nan, 64, 64, FillRule::NonZero).data.iter().all(|v| *v == 0.0));
    }

    use super::*;

    #[test]
    fn roundtrip_vertex_model() {
        let sp = ShapePath::ellipse([10.0, 20.0], 40.0, 30.0);
        let k = to_kurbo(&sp);
        let back = from_kurbo(&k);
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert!(b.closed);
        assert_eq!(b.vertices.len(), 4);
        for i in 0..4 {
            for c in 0..2 {
                assert!((b.vertices[i][c] - sp.vertices[i][c]).abs() < 1e-9);
                assert!((b.in_tangents[i][c] - sp.in_tangents[i][c]).abs() < 1e-9, "{i} {b:?}");
                assert!((b.out_tangents[i][c] - sp.out_tangents[i][c]).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn rect_coverage_area() {
        let r = rect([20.0, 10.0], [50.0, 50.0], 0.0);
        let m = fill_coverage(&[r], &Mat3::IDENTITY, 100, 100, FillRule::NonZero);
        let area: f32 = m.data.iter().sum();
        assert!((area - 200.0).abs() < 1.0, "{area}");
    }

    #[test]
    fn stroke_has_area() {
        let r = rect([40.0, 40.0], [50.0, 50.0], 0.0);
        let st = StrokeStyle { width: 4.0, ..Default::default() };
        let m = stroke_coverage(&[r], &st, &Mat3::IDENTITY, 100, 100);
        let area: f32 = m.data.iter().sum();
        assert!((area - 4.0 * 160.0).abs() < 40.0, "{area}");
    }

    #[test]
    fn star_has_right_vertex_count() {
        let p = polystar(true, 5.0, [0.0, 0.0], 0.0, 20.0, 50.0, 0.0, 0.0);
        let v = from_kurbo(&p);
        assert_eq!(v[0].vertices.len(), 10);
    }
}
