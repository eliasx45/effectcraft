//! The edges tiny-skia's anti-aliased path filler walks, for an accelerator to scan-convert with
//! the same samples (`effectcraft-gpu`'s `shapes_gpu`): the CPU's coverage masks come from
//! `tiny_skia::Mask::fill_path` (via [`effectcraft_path::fill_coverage`]), so the GPU matches them
//! by building the very same edges: the path made and transformed in `f32` as tiny-skia does,
//! curves turned into lines by its fixed-point forward differencing, every line as a 16.16
//! fixed-point x stepped per sub-scanline (4 per pixel row, 4 sample columns per pixel).
//!
//! Paths reaching outside the mask go through tiny-skia's edge clipper first (curves chopped
//! at its sides, what lies left or right as vertical lines there), and paths reaching far
//! outside are flattened and clipped as `fill_coverage` does, so every path is covered.
//!
//! Each [`ScanEdge`] covers up to [`CHUNK`] consecutive sub-scanlines; on sub-scanline `y` its
//! sample column is `(x + k·dx + 0x8000) >> 16` (`k = y − first`), and every sample at or right
//! of it gets the edge's winding. A pixel's coverage is then tiny-skia's `SuperBlitter` sum: per
//! sub-scanline, 16 per covered sample, or 64 (63 on the last sub-scanline) when all four are
//! covered, capped at 255.
//!
//! Ported from tiny-skia 0.12 (`edge.rs`, `edge_builder.rs`, `path_geometry.rs`,
//! `edge_clipper.rs`, `line_clipper.rs`, `fixed_point.rs`, `scan/path_aa.rs`, and `tiny-skia-path`'s `path_builder.rs`,
//! `path_geometry.rs`, `transform.rs`), itself a port of Skia, under this licence:
//!
//! Copyright (c) 2011 Google Inc. All rights reserved.
//! Copyright (c) 2020 Yevhenii Reizner All rights reserved.
//! Copyright 2006, 2009 The Android Open Source Project.
//!
//! Redistribution and use in source and binary forms, with or without modification, are
//! permitted provided that the following conditions are met:
//!
//!   * Redistributions of source code must retain the above copyright notice, this list of
//!     conditions and the following disclaimer.
//!   * Redistributions in binary form must reproduce the above copyright notice, this list of
//!     conditions and the following disclaimer in the documentation and/or other materials
//!     provided with the distribution.
//!   * Neither the name of the copyright holder nor the names of its contributors may be used
//!     to endorse or promote products derived from this software without specific prior
//!     written permission.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS
//! OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF
//! MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE
//! COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
//! EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE
//! GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
//! ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
//! NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED
//! OF THE POSSIBILITY OF SUCH DAMAGE.

use effectcraft_geom::Mat3;
use effectcraft_path::BezPath;
use kurbo::PathEl;

/// Sub-scanlines one [`ScanEdge`] spans at most (one GPU invocation each).
pub const CHUNK: i32 = 32;
/// Scan edges a draw may have; more renders on the CPU.
pub const MAX_EDGES: usize = 4 << 20;
/// Supersampling shift (4 × 4 samples per pixel), as tiny-skia's `SUPERSAMPLE_SHIFT`.
const SHIFT: i32 = 2;
/// How far past the mask points may lie before the CPU clips the path first
/// (`effectcraft_path`'s `SAFE_REACH`); such paths stay on the CPU here.
const SAFE_REACH: f64 = 4096.0;
/// Largest mask side tiny-skia fills in one tile (`DrawTiler`) with anti-aliasing.
const MAX_SIDE: u32 = 8191;
const MAX_COEFF_SHIFT: i32 = 6;
const NEARLY_ZERO: f32 = 1.0 / 4096.0;

/// `[first sub-scanline, (count << 1) | (winding < 0), x at the first (16.16), dx per sub-scanline (16.16)]`.
pub type ScanEdge = [i32; 4];

/// What filling a path into a `w × h` mask gives.
#[derive(Debug, PartialEq)]
pub enum Scan {
    /// Nothing is drawn (as tiny-skia: an empty, degenerate or non-finite path).
    Empty,
    /// The edges to walk.
    Edges(Vec<ScanEdge>),
    /// Not reproduced here (a mask tiny-skia tiles, over 8191 px a side; a projective matrix;
    /// too many edges): the CPU must rasterise this one.
    Decline,
}

type Pt = [f32; 2];

#[derive(Clone, Copy, PartialEq)]
enum Verb {
    Move,
    Line,
    Quad,
    Cubic,
    Close,
}

/// tiny-skia's `PathBuilder`, fed as `effectcraft_path`'s `to_skia` feeds it.
struct Builder {
    verbs: Vec<Verb>,
    pts: Vec<Pt>,
    last_move: usize,
    move_required: bool,
}

impl Builder {
    fn new() -> Self {
        Builder { verbs: Vec::new(), pts: Vec::new(), last_move: 0, move_required: true }
    }

    fn move_to(&mut self, p: Pt) {
        if self.verbs.last() == Some(&Verb::Move) {
            if let Some(last) = self.pts.last_mut() {
                *last = p;
            }
        } else {
            self.last_move = self.pts.len();
            self.move_required = false;
            self.verbs.push(Verb::Move);
            self.pts.push(p);
        }
    }

    fn inject(&mut self) {
        if self.move_required {
            let p = self.pts.get(self.last_move).copied().unwrap_or([0.0, 0.0]);
            self.move_to(p);
        }
    }

    fn push(&mut self, v: Verb, pts: &[Pt]) {
        self.inject();
        self.verbs.push(v);
        self.pts.extend_from_slice(pts);
    }

    fn close(&mut self) {
        if !self.verbs.is_empty() && self.verbs.last() != Some(&Verb::Close) {
            self.verbs.push(Verb::Close);
        }
        self.move_required = true;
    }
}

/// `Rect::from_points`: `[left, top, right, bottom]`, `None` when not finite.
fn bounds(pts: &[Pt]) -> Option<[f32; 4]> {
    let first = pts.first()?;
    let mut b = [first[0], first[1], first[0], first[1]];
    for p in pts {
        if !(p[0].is_finite() && p[1].is_finite()) {
            return None;
        }
        b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
    }
    ((b[2] - b[0]).is_finite() && (b[3] - b[1]).is_finite()).then_some(b)
}

/// The scan edges of `paths` (one path, as `fill_coverage` joins them) mapped by `m` into a
/// `w × h` mask.
pub fn scan_edges(paths: &[BezPath], m: &Mat3, w: u32, h: u32) -> Scan {
    let (w, h) = (w.max(1), h.max(1));
    if w > MAX_SIDE || h > MAX_SIDE || !m.is_affine() {
        return Scan::Decline;
    }
    let a = &m.0;
    // `fill_coverage`'s extent test, in f64.
    let (wf, hf) = (f64::from(w), f64::from(h));
    let mut huge = false;
    for p in paths {
        for el in p.elements() {
            let pts: &[kurbo::Point] = match el {
                PathEl::MoveTo(p) | PathEl::LineTo(p) => std::slice::from_ref(p),
                PathEl::QuadTo(a, p) => &[*a, *p],
                PathEl::CurveTo(a, b, p) => &[*a, *b, *p],
                PathEl::ClosePath => &[],
            };
            for q in pts {
                let x = a[0][0] * q.x + a[0][1] * q.y + a[0][2];
                let y = a[1][0] * q.x + a[1][1] * q.y + a[1][2];
                if !(x.is_finite() && y.is_finite()) {
                    return Scan::Empty;
                }
                huge |= x < -SAFE_REACH || y < -SAFE_REACH || x > wf + SAFE_REACH || y > hf + SAFE_REACH;
            }
        }
    }
    if huge {
        // As `fill_coverage`: flattened and clipped in device space first, filled untransformed.
        let to_device = kurbo::Affine::new([a[0][0], a[1][0], a[0][1], a[1][1], a[0][2], a[1][2]]);
        let mut all = BezPath::new();
        for p in paths {
            all.extend(p.elements().iter().copied());
        }
        let clipped = clip_to_rect(&(to_device * all), w, h);
        return scan_device(std::slice::from_ref(&clipped), &Mat3::IDENTITY, w, h);
    }
    scan_device(paths, m, w, h)
}

/// [`scan_edges`] of paths near the mask (tiny-skia's part of `fill_coverage`).
fn scan_device(paths: &[BezPath], m: &Mat3, w: u32, h: u32) -> Scan {
    let a = &m.0;
    // `to_skia`: the path in f32 through tiny-skia's builder.
    let mut b = Builder::new();
    let f = |p: kurbo::Point| [p.x as f32, p.y as f32];
    for p in paths {
        for el in p.elements() {
            match *el {
                PathEl::MoveTo(p) => b.move_to(f(p)),
                PathEl::LineTo(p) => b.push(Verb::Line, &[f(p)]),
                PathEl::QuadTo(c, p) => b.push(Verb::Quad, &[f(c), f(p)]),
                PathEl::CurveTo(c, d, p) => b.push(Verb::Cubic, &[f(c), f(d), f(p)]),
                PathEl::ClosePath => b.close(),
            }
        }
    }
    if b.verbs.len() <= 1 || bounds(&b.pts).is_none() {
        return Scan::Empty;
    }
    // `Path::transform` (`Transform::from_row(sx, ky, kx, sy, tx, ty)`, mapped in f32).
    let (sx, ky, kx, sy, tx, ty) = (a[0][0] as f32, a[1][0] as f32, a[0][1] as f32, a[1][1] as f32, a[0][2] as f32, a[1][2] as f32);
    let identity = sx == 1.0 && ky == 0.0 && kx == 0.0 && sy == 1.0 && tx == 0.0 && ty == 0.0;
    let mut pts = b.pts;
    if !identity {
        for p in &mut pts {
            let (x, y) = (p[0], p[1]);
            *p = [x * sx + y * kx + tx, x * ky + y * sy + ty];
        }
    }
    let Some([l, t, r, btm]) = bounds(&pts) else { return Scan::Empty };
    // `Mask::fill_path`: skip empty paths and horizontal/vertical lines, and huge coordinates.
    if (r - l).abs() <= NEARLY_ZERO || (btm - t).abs() <= NEARLY_ZERO {
        return Scan::Empty;
    }
    const MAX: f32 = f32::MAX * 0.25;
    if !(l >= -MAX && t >= -MAX && r <= MAX && btm <= MAX) {
        return Scan::Empty;
    }
    // `path_aa::fill_path`: the rounded-out bounds, clipped to the mask.
    let ir = [l.floor() as i32, t.floor() as i32, r.ceil() as i32, btm.ceil() as i32];
    if ir[2] <= ir[0] || ir[3] <= ir[1] {
        return Scan::Empty;
    }
    let (wi, hi) = (w as i32, h as i32);
    if ir[2] <= 0 || ir[3] <= 0 || ir[0] >= wi || ir[1] >= hi {
        return Scan::Empty;
    }
    // `fill_path_impl`: a path reaching outside the mask has its edges clipped to it first
    // (`EdgeClipper`: curves chopped there, what lies left or right becomes vertical lines on
    // the mask's sides); the clipped curves are already monotonic.
    let contained = ir[0] >= 0 && ir[1] >= 0 && ir[2] <= wi && ir[3] <= hi;
    let edges = if contained {
        path_edges(&b.verbs, &pts)
    } else {
        let mut clipped = Vec::new();
        let c = [0.0, 0.0, w as f32, h as f32];
        for e in path_edges(&b.verbs, &pts) {
            clip_edge(e, c, &mut clipped);
            if clipped.len() > MAX_EDGES {
                return Scan::Decline;
            }
        }
        // `BasicEdgeBuilder::build`: a non-finite clipped point fails the whole path.
        let finite = |p: &Pt| p[0].is_finite() && p[1].is_finite();
        if !clipped.iter().all(|e| match e {
            PathEdge::Line(a, b) => finite(a) && finite(b),
            PathEdge::Quad(a, b, c) => finite(a) && finite(b) && finite(c),
            PathEdge::Cubic(a, b, c, d) => finite(a) && finite(b) && finite(c) && finite(d),
        }) {
            return Scan::Empty;
        }
        clipped
    };
    // `BasicEdgeBuilder`'s edge list: lines kept (a vertical one merges with a vertical line
    // before it, as `combine_vertical` does), curves walked into chunks straight away.
    let mut out = Vec::new();
    let mut list: Vec<Option<LineEdge>> = Vec::new();
    for e in edges {
        match e {
            PathEdge::Line(p0, p1) => {
                if let Some(line) = LineEdge::new(p0, p1, SHIFT) {
                    push_line(line, &mut list);
                }
            }
            PathEdge::Quad(p0, p1, p2) => {
                let (n, mono) = if contained { chop_quad_at_extrema(&[p0, p1, p2], 1) } else { (0, [p0, p1, p2, [0.0; 2], [0.0; 2]]) };
                for i in 0..=n {
                    let Some(q) = mono.get(i * 2..i * 2 + 3) else { continue };
                    let Some(mut quad) = QuadEdge::new([q[0], q[1], q[2]], SHIFT) else { continue };
                    list.push(None);
                    emit(&quad.line, &mut out);
                    while quad.count > 0 && quad.update() {
                        emit(&quad.line, &mut out);
                    }
                }
            }
            PathEdge::Cubic(p0, p1, p2, p3) => {
                let (n, mono) = if contained {
                    chop_cubic_at_extrema(&[p0, p1, p2, p3], 1)
                } else {
                    let mut d = [[0.0f32; 2]; 10];
                    d[..4].copy_from_slice(&[p0, p1, p2, p3]);
                    (0, d)
                };
                for i in 0..=n {
                    let Some(c) = mono.get(i * 3..i * 3 + 4) else { continue };
                    let Some(mut cubic) = CubicEdge::new([c[0], c[1], c[2], c[3]], SHIFT) else { continue };
                    list.push(None);
                    emit(&cubic.line, &mut out);
                    while cubic.count < 0 && cubic.update() {
                        emit(&cubic.line, &mut out);
                    }
                }
            }
        }
        if out.len().saturating_add(list.len()) > MAX_EDGES {
            return Scan::Decline;
        }
    }
    // `BasicEdgeBuilder::build_edges`: fewer than two edges draw nothing.
    if list.len() < 2 {
        return Scan::Empty;
    }
    for line in list.iter().flatten() {
        emit(line, &mut out);
        if out.len() > MAX_EDGES {
            return Scan::Decline;
        }
    }
    if out.is_empty() { Scan::Empty } else { Scan::Edges(out) }
}

/// `BasicEdgeBuilder::push_line` (`None` in `list` is a curve).
fn push_line(edge: LineEdge, list: &mut Vec<Option<LineEdge>>) {
    if edge.dx == 0
        && let Some(Some(last)) = list.last_mut()
    {
        match combine_vertical(&edge, last) {
            Combine::Total => {
                list.pop();
                return;
            }
            Combine::Partial => return,
            Combine::No => {}
        }
    }
    list.push(Some(edge));
}

enum Combine {
    No,
    Partial,
    Total,
}

/// `combine_vertical`.
fn combine_vertical(edge: &LineEdge, last: &mut LineEdge) -> Combine {
    if last.dx != 0 || edge.x != last.x {
        return Combine::No;
    }
    if edge.winding == last.winding {
        return if edge.last_y.wrapping_add(1) == last.first_y {
            last.first_y = edge.first_y;
            Combine::Partial
        } else if edge.first_y == last.last_y.wrapping_add(1) {
            last.last_y = edge.last_y;
            Combine::Partial
        } else {
            Combine::No
        };
    }
    if edge.first_y == last.first_y {
        return if edge.last_y == last.last_y {
            Combine::Total
        } else if edge.last_y < last.last_y {
            last.first_y = edge.last_y.wrapping_add(1);
            Combine::Partial
        } else {
            last.first_y = last.last_y.wrapping_add(1);
            last.last_y = edge.last_y;
            last.winding = edge.winding;
            Combine::Partial
        };
    }
    if edge.last_y == last.last_y {
        if edge.first_y > last.first_y {
            last.last_y = edge.first_y.wrapping_sub(1);
        } else {
            last.last_y = last.first_y.wrapping_sub(1);
            last.first_y = edge.first_y;
            last.winding = edge.winding;
        }
        return Combine::Partial;
    }
    Combine::No
}

/// Add a line edge, cut into [`CHUNK`]s of sub-scanlines (`x` stepped as the walk steps it).
fn emit(line: &LineEdge, out: &mut Vec<ScanEdge>) {
    let mut first = line.first_y;
    while first <= line.last_y {
        let n = (line.last_y - first + 1).min(CHUNK);
        let k = first.wrapping_sub(line.first_y);
        let x = line.x.wrapping_add(line.dx.wrapping_mul(k));
        out.push([first, (n << 1) | i32::from(line.winding < 0), x, line.dx]);
        first += n;
    }
}

enum PathEdge {
    Line(Pt, Pt),
    Quad(Pt, Pt, Pt),
    Cubic(Pt, Pt, Pt, Pt),
}

/// `edge_builder::PathEdgeIter`: every segment, each contour closed with a line.
fn path_edges(verbs: &[Verb], pts: &[Pt]) -> Vec<PathEdge> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut move_to = [0.0, 0.0];
    let mut needs_close = false;
    let at = |k: usize| pts.get(k).copied().unwrap_or([0.0, 0.0]);
    for v in verbs {
        match v {
            Verb::Move => {
                if needs_close {
                    out.push(PathEdge::Line(at(i.wrapping_sub(1)), move_to));
                    needs_close = false;
                }
                move_to = at(i);
                i += 1;
            }
            Verb::Close => {
                if needs_close {
                    out.push(PathEdge::Line(at(i.wrapping_sub(1)), move_to));
                    needs_close = false;
                }
            }
            Verb::Line => {
                needs_close = true;
                out.push(PathEdge::Line(at(i.wrapping_sub(1)), at(i)));
                i += 1;
            }
            Verb::Quad => {
                needs_close = true;
                out.push(PathEdge::Quad(at(i.wrapping_sub(1)), at(i), at(i + 1)));
                i += 2;
            }
            Verb::Cubic => {
                needs_close = true;
                out.push(PathEdge::Cubic(at(i.wrapping_sub(1)), at(i), at(i + 1), at(i + 2)));
                i += 3;
            }
        }
    }
    if needs_close {
        out.push(PathEdge::Line(at(i.wrapping_sub(1)), move_to));
    }
    out
}

// ---------------------------------------------------------------- fixed point (fixed_point.rs, math.rs)

fn left_shift(v: i32, s: i32) -> i32 {
    ((v as u32).wrapping_shl(s as u32)) as i32
}

fn shr(v: i32, s: i32) -> i32 {
    v.wrapping_shr(s as u32)
}

fn fdot6_round(n: i32) -> i32 {
    shr(n.wrapping_add(32), 6)
}

fn fdot6_to_fdot16(n: i32) -> i32 {
    left_shift(n, 10)
}

fn fdot6_div(a: i32, b: i32) -> i32 {
    if b == 0 {
        return 0;
    }
    if i16::try_from(a).is_ok() {
        left_shift(a, 16).wrapping_div(b)
    } else {
        let v = ((a as i64 as u64).wrapping_shl(16) as i64).wrapping_div(i64::from(b));
        v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }
}

fn fdot16_mul(a: i32, b: i32) -> i32 {
    ((i64::from(a) * i64::from(b)) >> 16) as i32
}

fn compute_dy(top: i32, y0: i32) -> i32 {
    left_shift(top, 6).wrapping_add(32).wrapping_sub(y0)
}

fn cheap_distance(dx: i32, dy: i32) -> i32 {
    let (dx, dy) = (dx.wrapping_abs(), dy.wrapping_abs());
    if dx > dy { dx.wrapping_add(dy >> 1) } else { dy.wrapping_add(dx >> 1) }
}

fn diff_to_shift(dx: i32, dy: i32, shift_aa: i32) -> i32 {
    let dist = cheap_distance(dx, dy);
    let dist = shr(dist.wrapping_add(left_shift(1, 2 + shift_aa)), 3 + shift_aa);
    (32 - dist.leading_zeros() as i32) >> 1
}

fn cubic_delta_from_line(a: i32, b: i32, c: i32, d: i32) -> i32 {
    let m = |x: i32, k: i32| x.wrapping_mul(k);
    let one = shr(m(m(a, 8).wrapping_sub(m(b, 15)).wrapping_add(m(c, 6)).wrapping_add(d), 19), 9);
    let two = shr(m(a.wrapping_add(m(b, 6)).wrapping_sub(m(c, 15)).wrapping_add(m(d, 8)), 19), 9);
    one.wrapping_abs().max(two.wrapping_abs())
}

// ---------------------------------------------------------------- edges (edge.rs)

#[derive(Clone, Copy, Debug, Default)]
struct LineEdge {
    x: i32,
    dx: i32,
    first_y: i32,
    last_y: i32,
    winding: i8,
}

impl LineEdge {
    fn new(p0: Pt, p1: Pt, shift: i32) -> Option<Self> {
        let scale = (1 << (shift + 6)) as f32;
        let (mut x0, mut y0) = ((p0[0] * scale) as i32, (p0[1] * scale) as i32);
        let (mut x1, mut y1) = ((p1[0] * scale) as i32, (p1[1] * scale) as i32);
        let mut winding = 1;
        if y0 > y1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
            winding = -1;
        }
        let top = fdot6_round(y0);
        let bottom = fdot6_round(y1);
        if top == bottom {
            return None;
        }
        let slope = fdot6_div(x1.wrapping_sub(x0), y1.wrapping_sub(y0));
        let dy = compute_dy(top, y0);
        Some(LineEdge { x: fdot6_to_fdot16(x0.wrapping_add(fdot16_mul(slope, dy))), dx: slope, first_y: top, last_y: bottom - 1, winding })
    }

    fn update(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
        let (y0, y1) = (y0 >> 10, y1 >> 10);
        let top = fdot6_round(y0);
        let bottom = fdot6_round(y1);
        if top == bottom {
            return false;
        }
        let (x0, x1) = (x0 >> 10, x1 >> 10);
        let slope = fdot6_div(x1.wrapping_sub(x0), y1.wrapping_sub(y0));
        let dy = compute_dy(top, y0);
        self.x = fdot6_to_fdot16(x0.wrapping_add(fdot16_mul(slope, dy)));
        self.dx = slope;
        self.first_y = top;
        self.last_y = bottom - 1;
        true
    }
}

struct QuadEdge {
    line: LineEdge,
    count: i32,
    shift: i32,
    qx: i32,
    qy: i32,
    qdx: i32,
    qdy: i32,
    qddx: i32,
    qddy: i32,
    last_x: i32,
    last_y: i32,
}

impl QuadEdge {
    fn new(p: [Pt; 3], shift: i32) -> Option<Self> {
        let scale = (1 << (shift + 6)) as f32;
        let (mut x0, mut y0) = ((p[0][0] * scale) as i32, (p[0][1] * scale) as i32);
        let (x1, y1) = ((p[1][0] * scale) as i32, (p[1][1] * scale) as i32);
        let (mut x2, mut y2) = ((p[2][0] * scale) as i32, (p[2][1] * scale) as i32);
        let mut winding = 1;
        if y0 > y2 {
            std::mem::swap(&mut x0, &mut x2);
            std::mem::swap(&mut y0, &mut y2);
            winding = -1;
        }
        let top = fdot6_round(y0);
        let bottom = fdot6_round(y2);
        if top == bottom {
            return None;
        }
        let dx = shr(left_shift(x1, 1).wrapping_sub(x0).wrapping_sub(x2), 2);
        let dy = shr(left_shift(y1, 1).wrapping_sub(y0).wrapping_sub(y2), 2);
        let mut shift = diff_to_shift(dx, dy, shift);
        if shift == 0 {
            shift = 1;
        } else if shift > MAX_COEFF_SHIFT {
            shift = MAX_COEFF_SHIFT;
        }
        let div2 = |v: i32| left_shift(v, 16 - 6 - 1);
        let a = div2(x0.wrapping_sub(x1).wrapping_sub(x1).wrapping_add(x2));
        let b = fdot6_to_fdot16(x1.wrapping_sub(x0));
        let (qx, qdx, qddx) = (fdot6_to_fdot16(x0), b.wrapping_add(shr(a, shift)), shr(a, shift - 1));
        let a = div2(y0.wrapping_sub(y1).wrapping_sub(y1).wrapping_add(y2));
        let b = fdot6_to_fdot16(y1.wrapping_sub(y0));
        let (qy, qdy, qddy) = (fdot6_to_fdot16(y0), b.wrapping_add(shr(a, shift)), shr(a, shift - 1));
        let mut q = QuadEdge {
            line: LineEdge { winding, ..LineEdge::default() },
            count: 1 << shift,
            shift: shift - 1,
            qx,
            qy,
            qdx,
            qdy,
            qddx,
            qddy,
            last_x: fdot6_to_fdot16(x2),
            last_y: fdot6_to_fdot16(y2),
        };
        q.update().then_some(q)
    }

    fn update(&mut self) -> bool {
        let mut count = self.count;
        let (mut oldx, mut oldy, mut dx, mut dy) = (self.qx, self.qy, self.qdx, self.qdy);
        let (mut newx, mut newy);
        let mut success;
        loop {
            count -= 1;
            if count > 0 {
                newx = oldx.wrapping_add(shr(dx, self.shift));
                dx = dx.wrapping_add(self.qddx);
                newy = oldy.wrapping_add(shr(dy, self.shift));
                dy = dy.wrapping_add(self.qddy);
            } else {
                newx = self.last_x;
                newy = self.last_y;
            }
            success = self.line.update(oldx, oldy, newx, newy);
            oldx = newx;
            oldy = newy;
            if count <= 0 || success {
                break;
            }
        }
        self.qx = newx;
        self.qy = newy;
        self.qdx = dx;
        self.qdy = dy;
        self.count = count;
        success
    }
}

struct CubicEdge {
    line: LineEdge,
    count: i32,
    shift: i32,
    dshift: i32,
    cx: i32,
    cy: i32,
    cdx: i32,
    cdy: i32,
    cddx: i32,
    cddy: i32,
    cdddx: i32,
    cdddy: i32,
    last_x: i32,
    last_y: i32,
}

impl CubicEdge {
    fn new(p: [Pt; 4], shift: i32) -> Option<Self> {
        let scale = (1 << (shift + 6)) as f32;
        let c = |q: Pt| ((q[0] * scale) as i32, (q[1] * scale) as i32);
        let ((mut x0, mut y0), (mut x1, mut y1), (mut x2, mut y2), (mut x3, mut y3)) = (c(p[0]), c(p[1]), c(p[2]), c(p[3]));
        let mut winding = 1;
        if y0 > y3 {
            std::mem::swap(&mut x0, &mut x3);
            std::mem::swap(&mut x1, &mut x2);
            std::mem::swap(&mut y0, &mut y3);
            std::mem::swap(&mut y1, &mut y2);
            winding = -1;
        }
        let top = fdot6_round(y0);
        let bot = fdot6_round(y3);
        if top == bot {
            return None;
        }
        let dx = cubic_delta_from_line(x0, x1, x2, x3);
        let dy = cubic_delta_from_line(y0, y1, y2, y3);
        let mut shift = diff_to_shift(dx, dy, 2) + 1;
        if shift > MAX_COEFF_SHIFT {
            shift = MAX_COEFF_SHIFT;
        }
        let mut up_shift = 6;
        let mut down_shift = shift + up_shift - 10;
        if down_shift < 0 {
            down_shift = 0;
            up_shift = 10 - shift;
        }
        let m3 = |v: i32| v.wrapping_mul(3);
        let coeffs = |v0: i32, v1: i32, v2: i32, v3: i32| {
            let b = left_shift(m3(v1.wrapping_sub(v0)), up_shift);
            let c = left_shift(m3(v0.wrapping_sub(v1).wrapping_sub(v1).wrapping_add(v2)), up_shift);
            let d = left_shift(v3.wrapping_add(m3(v1.wrapping_sub(v2))).wrapping_sub(v0), up_shift);
            let d1 = b.wrapping_add(shr(c, shift)).wrapping_add(shr(d, 2 * shift));
            let d2 = c.wrapping_mul(2).wrapping_add(shr(m3(d), shift - 1));
            let d3 = shr(m3(d), shift - 1);
            (fdot6_to_fdot16(v0), d1, d2, d3)
        };
        let (cx, cdx, cddx, cdddx) = coeffs(x0, x1, x2, x3);
        let (cy, cdy, cddy, cdddy) = coeffs(y0, y1, y2, y3);
        let mut e = CubicEdge {
            line: LineEdge { winding, ..LineEdge::default() },
            count: -(1 << shift),
            shift,
            dshift: down_shift,
            cx,
            cy,
            cdx,
            cdy,
            cddx,
            cddy,
            cdddx,
            cdddy,
            last_x: fdot6_to_fdot16(x3),
            last_y: fdot6_to_fdot16(y3),
        };
        e.update().then_some(e)
    }

    fn update(&mut self) -> bool {
        let mut count = self.count;
        let (mut oldx, mut oldy) = (self.cx, self.cy);
        let (mut newx, mut newy);
        let mut success;
        loop {
            count += 1;
            if count < 0 {
                newx = oldx.wrapping_add(shr(self.cdx, self.dshift));
                self.cdx = self.cdx.wrapping_add(shr(self.cddx, self.shift));
                self.cddx = self.cddx.wrapping_add(self.cdddx);
                newy = oldy.wrapping_add(shr(self.cdy, self.dshift));
                self.cdy = self.cdy.wrapping_add(shr(self.cddy, self.shift));
                self.cddy = self.cddy.wrapping_add(self.cdddy);
            } else {
                newx = self.last_x;
                newy = self.last_y;
            }
            if newy < oldy {
                newy = oldy;
            }
            success = self.line.update(oldx, oldy, newx, newy);
            oldx = newx;
            oldy = newy;
            if count >= 0 || success {
                break;
            }
        }
        self.cx = newx;
        self.cy = newy;
        self.count = count;
        success
    }
}

// ---------------------------------------------------------------- curve chopping (path_geometry.rs)

fn interp(a: Pt, b: Pt, t: f32) -> Pt {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// `valid_unit_divide`: `numer / denom` when strictly inside (0, 1).
fn unit_divide(mut numer: f32, mut denom: f32) -> Option<f32> {
    if numer < 0.0 {
        numer = -numer;
        denom = -denom;
    }
    if denom == 0.0 || numer == 0.0 || numer >= denom {
        return None;
    }
    let r = numer / denom;
    (r > 0.0 && r < 1.0).then_some(r)
}

fn chop_quad_at(src: &[Pt; 3], t: f32) -> [Pt; 5] {
    let p01 = interp(src[0], src[1], t);
    let p12 = interp(src[1], src[2], t);
    [src[0], p01, interp(p01, p12, t), p12, src[2]]
}

fn is_not_monotonic(a: f32, b: f32, c: f32) -> bool {
    let ab = a - b;
    let mut bc = b - c;
    if ab < 0.0 {
        bc = -bc;
    }
    ab == 0.0 || bc < 0.0
}

/// `chop_quad_at_x_extrema` (`axis` 0) / `chop_quad_at_y_extrema` (1).
fn chop_quad_at_extrema(src: &[Pt; 3], axis: usize) -> (usize, [Pt; 5]) {
    let k = axis.min(1);
    let (a, mut b, c) = (src[0][k], src[1][k], src[2][k]);
    if is_not_monotonic(a, b, c) {
        if let Some(t) = unit_divide(a - b, a - b - b + c) {
            let mut dst = chop_quad_at(src, t);
            dst[1][k] = dst[2][k];
            dst[3][k] = dst[2][k];
            return (1, dst);
        }
        b = if (a - b).abs() < (b - c).abs() { a } else { c };
    }
    let mut dst = [src[0], src[1], src[2], [0.0; 2], [0.0; 2]];
    dst[0][k] = a;
    dst[1][k] = b;
    dst[2][k] = c;
    (0, dst)
}

/// `find_unit_quad_roots`: roots of `a t² + b t + c` strictly inside (0, 1), ascending.
fn unit_quad_roots(a: f32, b: f32, c: f32) -> Vec<f32> {
    if a == 0.0 {
        return unit_divide(-c, b).into_iter().collect();
    }
    let mut dr = f64::from(b) * f64::from(b) - 4.0 * f64::from(a) * f64::from(c);
    if dr < 0.0 {
        return vec![];
    }
    dr = dr.sqrt();
    let r = dr as f32;
    if !r.is_finite() {
        return vec![];
    }
    let q = if b < 0.0 { -(b - r) / 2.0 } else { -(b + r) / 2.0 };
    let mut roots: Vec<f32> = unit_divide(q, a).into_iter().chain(unit_divide(c, q)).collect();
    if let [r0, r1] = roots[..] {
        if r0 > r1 {
            roots = vec![r1, r0];
        } else if r0 == r1 {
            roots.truncate(1);
        }
    }
    roots
}

fn chop_cubic_at2(src: &[Pt; 4], t: f32) -> [Pt; 7] {
    let ab = interp(src[0], src[1], t);
    let bc = interp(src[1], src[2], t);
    let cd = interp(src[2], src[3], t);
    let abc = interp(ab, bc, t);
    let bcd = interp(bc, cd, t);
    [src[0], ab, abc, interp(abc, bcd, t), bcd, cd, src[3]]
}

/// `chop_cubic_at_x_extrema` (`axis` 0) / `chop_cubic_at_y_extrema` (1).
fn chop_cubic_at_extrema(src: &[Pt; 4], axis: usize) -> (usize, [Pt; 10]) {
    let k = axis.min(1);
    let (a, b, c, d) = (src[0][k], src[1][k], src[2][k], src[3][k]);
    let ts = unit_quad_roots(d - a + 3.0 * (b - c), 2.0 * (a - b - b + c), b - a);
    let mut dst = [[0.0f32; 2]; 10];
    if ts.is_empty() {
        dst[..4].copy_from_slice(src);
        return (0, dst);
    }
    // `chop_cubic_at`.
    let mut cur = *src;
    let mut t = ts[0];
    let mut off = 0usize;
    for (i, &ti) in ts.iter().enumerate() {
        let piece = chop_cubic_at2(&cur, t);
        if let Some(slot) = dst.get_mut(off..off + 7) {
            slot.copy_from_slice(&piece);
        }
        if i + 1 == ts.len() {
            break;
        }
        off += 3;
        cur = [piece[3], piece[4], piece[5], piece[6]];
        match ts.get(i + 1).and_then(|&next| unit_divide(next - ti, 1.0 - ti)) {
            Some(n) => t = n,
            None => {
                for k in 4..7 {
                    if let Some(p) = dst.get_mut(off + k) {
                        *p = cur[3];
                    }
                }
                break;
            }
        }
    }
    dst[2][k] = dst[3][k];
    dst[4][k] = dst[3][k];
    if ts.len() == 2 {
        dst[5][k] = dst[6][k];
        dst[7][k] = dst[6][k];
    }
    (ts.len(), dst)
}

// ---------------------------------------------------------------- clipping (edge_clipper.rs, line_clipper.rs)

/// `[left, top, right, bottom]`.
type Clip = [f32; 4];

fn ave(a: f32, b: f32) -> f32 {
    (a + b) * 0.5
}

fn pin(v: f64, a: f64, b: f64) -> f64 {
    let (lo, hi) = if b < a { (b, a) } else { (a, b) };
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

fn sect_with_horizontal(src: &[Pt; 2], y: f32) -> f32 {
    let dy = src[1][1] - src[0][1];
    if dy.abs() <= NEARLY_ZERO {
        return ave(src[0][0], src[1][0]);
    }
    let (x0, y0, x1, y1) = (f64::from(src[0][0]), f64::from(src[0][1]), f64::from(src[1][0]), f64::from(src[1][1]));
    pin(x0 + (f64::from(y) - y0) * (x1 - x0) / (y1 - y0), x0, x1) as f32
}

fn sect_clamp_with_vertical(src: &[Pt; 2], x: f32) -> f32 {
    let dx = src[1][0] - src[0][0];
    let y = if dx.abs() <= NEARLY_ZERO {
        ave(src[0][1], src[1][1])
    } else {
        let (x0, y0, x1, y1) = (f64::from(src[0][0]), f64::from(src[0][1]), f64::from(src[1][0]), f64::from(src[1][1]));
        (y0 + (f64::from(x) - x0) * (y1 - y0) / (x1 - x0)) as f32
    };
    let (lo, hi) = if src[1][1] < src[0][1] { (src[1][1], src[0][1]) } else { (src[0][1], src[1][1]) };
    if y < lo {
        lo
    } else if y > hi {
        hi
    } else {
        y
    }
}

/// `line_clipper::clip` (nothing culled to the right) as lines.
fn clip_line(src: [Pt; 2], c: Clip, out: &mut Vec<PathEdge>) {
    let (i0, i1) = if src[0][1] < src[1][1] { (0, 1) } else { (1, 0) };
    if src[i1][1] <= c[1] || src[i0][1] >= c[3] {
        return;
    }
    let mut tmp = src;
    if src[i0][1] < c[1] {
        tmp[i0] = [sect_with_horizontal(&src, c[1]), c[1]];
    }
    if tmp[i1][1] > c[3] {
        tmp[i1] = [sect_with_horizontal(&src, c[3]), c[3]];
    }
    let (i0, i1, mut reverse) = if src[0][0] < src[1][0] { (0, 1, false) } else { (1, 0, true) };
    let mut res = [[0.0f32; 2]; 4];
    let count = if tmp[i1][0] <= c[0] || tmp[i0][0] >= c[2] {
        let x = if tmp[i1][0] <= c[0] { c[0] } else { c[2] };
        tmp[0][0] = x;
        tmp[1][0] = x;
        reverse = false;
        res[0] = tmp[0];
        res[1] = tmp[1];
        1
    } else {
        let mut at = 0;
        if tmp[i0][0] < c[0] {
            res[0] = [c[0], tmp[i0][1]];
            at = 1;
            res[1] = [c[0], sect_clamp_with_vertical(&tmp, c[0])];
        } else {
            res[0] = tmp[i0];
        }
        at += 1;
        if tmp[i1][0] > c[2] {
            res[at] = [c[2], sect_clamp_with_vertical(&tmp, c[2])];
            at += 1;
            res[at] = [c[2], tmp[i1][1]];
        } else {
            res[at] = tmp[i1];
        }
        at
    };
    let pts: Vec<Pt> = if reverse { res[..=count].iter().rev().copied().collect() } else { res[..=count].to_vec() };
    for w in pts.windows(2) {
        out.push(PathEdge::Line(w[0], w[1]));
    }
}

fn push_vline(x: f32, mut y0: f32, mut y1: f32, reverse: bool, out: &mut Vec<PathEdge>) {
    if reverse {
        std::mem::swap(&mut y0, &mut y1);
    }
    out.push(PathEdge::Line([x, y0], [x, y1]));
}

/// The first root of the monotonic quadratic `c0, c1, c2` at `target` (`chop_mono_quad_at`).
fn chop_mono_quad_at(c0: f32, c1: f32, c2: f32, target: f32) -> Option<f32> {
    unit_quad_roots(c0 - c1 - c1 + c2, 2.0 * (c1 - c0), c0 - target).first().copied()
}

/// `EdgeClipper`: one path edge clipped to `c`.
fn clip_edge(e: PathEdge, c: Clip, out: &mut Vec<PathEdge>) {
    match e {
        PathEdge::Line(p0, p1) => clip_line([p0, p1], c, out),
        PathEdge::Quad(p0, p1, p2) => {
            let pts = [p0, p1, p2];
            let Some(b) = bounds(&pts) else { return };
            if b[1] >= c[3] || b[3] <= c[1] {
                return;
            }
            let (ny, mono_y) = chop_quad_at_extrema(&pts, 1);
            for y in 0..=ny {
                let Some(q) = mono_y.get(y * 2..y * 2 + 3) else { continue };
                let (nx, mono_x) = chop_quad_at_extrema(&[q[0], q[1], q[2]], 0);
                for x in 0..=nx {
                    let Some(q) = mono_x.get(x * 2..x * 2 + 3) else { continue };
                    clip_mono_quad([q[0], q[1], q[2]], c, out);
                }
            }
        }
        PathEdge::Cubic(p0, p1, p2, p3) => {
            let pts = [p0, p1, p2, p3];
            let Some(b) = bounds(&pts) else { return };
            if !(b[3] > c[1] && b[1] < c[3]) {
                return;
            }
            let limit = (1 << 22) as f32;
            if b[0] < -limit || b[1] < -limit || b[2] > limit || b[3] > limit {
                clip_line([p0, p3], c, out);
                return;
            }
            let (ny, mono_y) = chop_cubic_at_extrema(&pts, 1);
            for y in 0..=ny {
                let Some(q) = mono_y.get(y * 3..y * 3 + 4) else { continue };
                let (nx, mono_x) = chop_cubic_at_extrema(&[q[0], q[1], q[2], q[3]], 0);
                for x in 0..=nx {
                    let Some(q) = mono_x.get(x * 3..x * 3 + 4) else { continue };
                    clip_mono_cubic([q[0], q[1], q[2], q[3]], c, out);
                }
            }
        }
    }
}

/// `src` monotonic in x and y.
fn clip_mono_quad(src: [Pt; 3], c: Clip, out: &mut Vec<PathEdge>) {
    let mut reverse = src[0][1] > src[2][1];
    let mut pts = if reverse { [src[2], src[1], src[0]] } else { src };
    if pts[2][1] <= c[1] || pts[0][1] >= c[3] {
        return;
    }
    // `chop_quad_in_y`.
    if pts[0][1] < c[1] {
        if let Some(t) = chop_mono_quad_at(pts[0][1], pts[1][1], pts[2][1], c[1]) {
            let mut tmp = chop_quad_at(&pts, t);
            tmp[2][1] = c[1];
            tmp[3][1] = tmp[3][1].max(c[1]);
            pts[0] = tmp[2];
            pts[1] = tmp[3];
        } else {
            for p in &mut pts {
                if p[1] < c[1] {
                    p[1] = c[1];
                }
            }
        }
    }
    if pts[2][1] > c[3] {
        if let Some(t) = chop_mono_quad_at(pts[0][1], pts[1][1], pts[2][1], c[3]) {
            let mut tmp = chop_quad_at(&pts, t);
            tmp[1][1] = tmp[1][1].min(c[3]);
            tmp[2][1] = c[3];
            pts[1] = tmp[1];
            pts[2] = tmp[2];
        } else {
            for p in &mut pts {
                if p[1] > c[3] {
                    p[1] = c[3];
                }
            }
        }
    }
    if pts[0][0] > pts[2][0] {
        pts.swap(0, 2);
        reverse = !reverse;
    }
    if pts[2][0] <= c[0] {
        push_vline(c[0], pts[0][1], pts[2][1], reverse, out);
        return;
    }
    if pts[0][0] >= c[2] {
        push_vline(c[2], pts[0][1], pts[2][1], reverse, out);
        return;
    }
    let push = |p: [Pt; 3], out: &mut Vec<PathEdge>| out.push(if reverse { PathEdge::Quad(p[2], p[1], p[0]) } else { PathEdge::Quad(p[0], p[1], p[2]) });
    if pts[0][0] < c[0] {
        match chop_mono_quad_at(pts[0][0], pts[1][0], pts[2][0], c[0]) {
            Some(t) => {
                let mut tmp = chop_quad_at(&pts, t);
                push_vline(c[0], tmp[0][1], tmp[2][1], reverse, out);
                tmp[2][0] = c[0];
                tmp[3][0] = tmp[3][0].max(c[0]);
                pts[0] = tmp[2];
                pts[1] = tmp[3];
            }
            None => {
                push_vline(c[0], pts[0][1], pts[2][1], reverse, out);
                return;
            }
        }
    }
    if pts[2][0] > c[2] {
        match chop_mono_quad_at(pts[0][0], pts[1][0], pts[2][0], c[2]) {
            Some(t) => {
                let mut tmp = chop_quad_at(&pts, t);
                tmp[1][0] = tmp[1][0].min(c[2]);
                tmp[2][0] = c[2];
                push([tmp[0], tmp[1], tmp[2]], out);
                push_vline(c[2], tmp[2][1], tmp[4][1], reverse, out);
            }
            None => {
                pts[1][0] = pts[1][0].min(c[2]);
                pts[2][0] = pts[2][0].min(c[2]);
                push(pts, out);
            }
        }
    } else {
        push(pts, out);
    }
}

/// `src` monotonic in x and y.
fn clip_mono_cubic(src: [Pt; 4], c: Clip, out: &mut Vec<PathEdge>) {
    let mut reverse = src[0][1] > src[3][1];
    let mut pts = if reverse { [src[3], src[2], src[1], src[0]] } else { src };
    if pts[3][1] <= c[1] || pts[0][1] >= c[3] {
        return;
    }
    // `chop_cubic_in_y`.
    if pts[0][1] < c[1] {
        let mut tmp = chop_mono_cubic_at(&pts, c[1], 1);
        if tmp[3][1] < c[1] && tmp[4][1] < c[1] && tmp[5][1] < c[1] {
            tmp = chop_mono_cubic_at(&[tmp[3], tmp[4], tmp[5], tmp[6]], c[1], 1);
        }
        tmp[3][1] = c[1];
        tmp[4][1] = tmp[4][1].max(c[1]);
        pts[0] = tmp[3];
        pts[1] = tmp[4];
        pts[2] = tmp[5];
    }
    if pts[3][1] > c[3] {
        let mut tmp = chop_mono_cubic_at(&pts, c[3], 1);
        tmp[3][1] = c[3];
        tmp[2][1] = tmp[2][1].min(c[3]);
        pts[1] = tmp[1];
        pts[2] = tmp[2];
        pts[3] = tmp[3];
    }
    if pts[0][0] > pts[3][0] {
        pts.swap(0, 3);
        pts.swap(1, 2);
        reverse = !reverse;
    }
    if pts[3][0] <= c[0] {
        push_vline(c[0], pts[0][1], pts[3][1], reverse, out);
        return;
    }
    if pts[0][0] >= c[2] {
        push_vline(c[2], pts[0][1], pts[3][1], reverse, out);
        return;
    }
    if pts[0][0] < c[0] {
        let mut tmp = chop_mono_cubic_at(&pts, c[0], 0);
        push_vline(c[0], tmp[0][1], tmp[3][1], reverse, out);
        tmp[3][0] = c[0];
        tmp[4][0] = tmp[4][0].max(c[0]);
        pts[0] = tmp[3];
        pts[1] = tmp[4];
        pts[2] = tmp[5];
    }
    let push =
        |p: [Pt; 4], out: &mut Vec<PathEdge>| out.push(if reverse { PathEdge::Cubic(p[3], p[2], p[1], p[0]) } else { PathEdge::Cubic(p[0], p[1], p[2], p[3]) });
    if pts[3][0] > c[2] {
        let mut tmp = chop_mono_cubic_at(&pts, c[2], 0);
        tmp[3][0] = c[2];
        tmp[2][0] = tmp[2][0].min(c[2]);
        push([tmp[0], tmp[1], tmp[2], tmp[3]], out);
        push_vline(c[2], tmp[3][1], tmp[6][1], reverse, out);
    } else {
        push(pts, out);
    }
}

/// `chop_mono_cubic_at_x/y` of the edge clipper: at the first intercept `path64` finds
/// (`cubic_dchop_at_intercept`), else at `mono_cubic_closest_t`.
fn chop_mono_cubic_at(src: &[Pt; 4], v: f32, axis: usize) -> [Pt; 7] {
    let k = axis.min(1);
    let p: [[f64; 2]; 4] = src.map(|q| [f64::from(q[0]), f64::from(q[1])]);
    if let Some(t) = c64::intercept(&p, f64::from(v), k) {
        return chop_cubic64(&p, t);
    }
    chop_cubic_at2(src, mono_cubic_closest_t([src[0][k], src[1][k], src[2][k], src[3][k]], v))
}

/// tiny-skia's `path64`: Skia's double-precision cubic intercepts.
mod c64 {
    const FLT_EPSILON_HALF: f64 = (f32::EPSILON / 2.0) as f64;
    const FLT_EPSILON_CUBED: f64 = (f32::EPSILON * f32::EPSILON * f32::EPSILON) as f64;
    const FLT_EPSILON_INVERSE: f64 = 1.0 / f32::EPSILON as f64;
    use std::f64::consts::PI;

    type P = [[f64; 2]; 4];

    fn bound(v: f64, min: f64, max: f64) -> f64 {
        max.min(v).max(min)
    }
    fn between(v: f64, a: f64, b: f64) -> bool {
        (a - v) * (b - v) <= 0.0
    }
    fn approximately_zero(v: f64) -> bool {
        v.abs() < f64::EPSILON
    }
    fn approximately_zero_or_more(v: f64) -> bool {
        v > -f64::EPSILON
    }
    fn approximately_one_or_less(v: f64) -> bool {
        v < 1.0 + f64::EPSILON
    }
    fn approximately_zero_inverse(v: f64) -> bool {
        v.abs() > FLT_EPSILON_INVERSE
    }
    fn approximately_zero_when_compared_to(v: f64, other: f64) -> bool {
        v == 0.0 || v.abs() < (other * f64::from(f32::EPSILON)).abs()
    }
    fn approximately_equal(a: f64, b: f64) -> bool {
        approximately_zero(a - b)
    }
    fn approximately_equal_half(a: f64, b: f64) -> bool {
        a - b < FLT_EPSILON_HALF
    }
    fn twos(x: f32) -> i32 {
        let b = x.to_bits() as i32;
        if b < 0 { (b & 0x7FFF_FFFF).wrapping_neg() } else { b }
    }
    fn almost_dequal_ulps(a: f64, b: f64) -> bool {
        if a.abs() < f64::from(f32::MAX) && b.abs() < f64::from(f32::MAX) {
            let (x, y) = (twos(a as f32), twos(b as f32));
            x < y.wrapping_add(16) && y < x.wrapping_add(16)
        } else {
            (a - b).abs() / a.abs().max(b.abs()) < f64::from(f32::EPSILON * 16.0)
        }
    }
    fn cube_root(x: f64) -> f64 {
        if x.abs() < FLT_EPSILON_CUBED {
            return 0.0;
        }
        let d = x.abs();
        let hi = (d.to_bits() >> 32) as u32;
        let mut a = f64::from_bits(u64::from((hi / 3).wrapping_add(715_094_163)) << 32);
        for _ in 0..3 {
            let a3 = a * a * a;
            a = a * (a3 + d + d) / (a3 + a3 + d);
        }
        if x < 0.0 { -a } else { a }
    }

    fn push_valid_ts(s: &[f64], t: &mut Vec<f64>) {
        'outer: for &v in s {
            if approximately_zero_or_more(v) && approximately_one_or_less(v) {
                let v = bound(v, 0.0, 1.0);
                for &u in t.iter() {
                    if approximately_equal(u, v) {
                        continue 'outer;
                    }
                }
                t.push(v);
            }
        }
    }

    fn quad_handle_zero(b: f64, c: f64) -> Vec<f64> {
        if approximately_zero(b) { if c == 0.0 { vec![0.0] } else { vec![] } } else { vec![-c / b] }
    }

    fn quad_roots_real(a: f64, b: f64, c: f64) -> Vec<f64> {
        if a == 0.0 {
            return quad_handle_zero(b, c);
        }
        let p = b / (2.0 * a);
        let q = c / a;
        if approximately_zero(a) && (approximately_zero_inverse(p) || approximately_zero_inverse(q)) {
            return quad_handle_zero(b, c);
        }
        let p2 = p * p;
        if !almost_dequal_ulps(p2, q) && p2 < q {
            return vec![];
        }
        let sqrt_d = if p2 > q { (p2 - q).sqrt() } else { 0.0 };
        let (s0, s1) = (sqrt_d - p, -sqrt_d - p);
        if almost_dequal_ulps(s0, s1) { vec![s0] } else { vec![s0, s1] }
    }

    fn quad_roots_valid_t(a: f64, b: f64, c: f64) -> Vec<f64> {
        let mut t = Vec::new();
        push_valid_ts(&quad_roots_real(a, b, c), &mut t);
        t
    }

    fn roots_real(a: f64, b: f64, c: f64, d: f64) -> Vec<f64> {
        if approximately_zero(a)
            && approximately_zero_when_compared_to(a, b)
            && approximately_zero_when_compared_to(a, c)
            && approximately_zero_when_compared_to(a, d)
        {
            return quad_roots_real(b, c, d);
        }
        if approximately_zero_when_compared_to(d, a) && approximately_zero_when_compared_to(d, b) && approximately_zero_when_compared_to(d, c) {
            let mut s = quad_roots_real(a, b, c);
            if s.iter().any(|&v| approximately_zero(v)) {
                return s;
            }
            s.push(0.0);
            return s;
        }
        if approximately_zero(a + b + c + d) {
            let mut s = quad_roots_real(a, a + b, -d);
            if s.iter().any(|&v| almost_dequal_ulps(v, 1.0)) {
                return s;
            }
            s.push(1.0);
            return s;
        }
        let inv_a = 1.0 / a;
        let (a, b, c) = (b * inv_a, c * inv_a, d * inv_a);
        let a2 = a * a;
        let q = (a2 - b * 3.0) / 9.0;
        let r = (2.0 * a2 * a - 9.0 * a * b + 27.0 * c) / 54.0;
        let r2 = r * r;
        let q3 = q * q * q;
        let r2_minus_q3 = r2 - q3;
        let adiv3 = a / 3.0;
        let mut s = Vec::with_capacity(3);
        if r2_minus_q3 < 0.0 {
            let theta = bound(r / q3.sqrt(), -1.0, 1.0).acos();
            let neg2_root_q = -2.0 * q.sqrt();
            s.push(neg2_root_q * (theta / 3.0).cos() - adiv3);
            let rr = neg2_root_q * ((theta + 2.0 * PI) / 3.0).cos() - adiv3;
            if !almost_dequal_ulps(s[0], rr) {
                s.push(rr);
            }
            let rr = neg2_root_q * ((theta - 2.0 * PI) / 3.0).cos() - adiv3;
            if !almost_dequal_ulps(s[0], rr) && (s.len() == 1 || !almost_dequal_ulps(s[1], rr)) {
                s.push(rr);
            }
        } else {
            let mut a = cube_root(r.abs() + r2_minus_q3.sqrt());
            if r > 0.0 {
                a = -a;
            }
            if a != 0.0 {
                a += q / a;
            }
            let r2 = a - adiv3;
            s.push(r2);
            if almost_dequal_ulps(r2, q3) {
                let r2 = -a / 2.0 - adiv3;
                if !almost_dequal_ulps(s[0], r2) {
                    s.push(r2);
                }
            }
        }
        s
    }

    fn roots_valid_t(a: f64, b: f64, c: f64, d: f64) -> Vec<f64> {
        let s = roots_real(a, b, c, d);
        let mut t = Vec::with_capacity(3);
        push_valid_ts(&s, &mut t);
        'outer: for &v in &s {
            if !approximately_one_or_less(v) && between(v, 1.0, 1.00005) {
                if t.iter().any(|&u| approximately_equal(u, 1.0)) {
                    continue 'outer;
                }
                t.push(1.0);
            } else if !approximately_zero_or_more(v) && between(v, -0.00005, 0.0) {
                if t.iter().any(|&u| approximately_equal(u, 0.0)) {
                    continue 'outer;
                }
                t.push(0.0);
            }
        }
        t
    }

    fn point_at_t(p: &P, t: f64) -> [f64; 2] {
        if t == 0.0 {
            return p[0];
        }
        if t == 1.0 {
            return p[3];
        }
        let one_t = 1.0 - t;
        let one_t2 = one_t * one_t;
        let a = one_t2 * one_t;
        let b = 3.0 * one_t2 * t;
        let t2 = t * t;
        let c = 3.0 * one_t * t2;
        let d = t2 * t;
        [a * p[0][0] + b * p[1][0] + c * p[2][0] + d * p[3][0], a * p[0][1] + b * p[1][1] + c * p[2][1] + d * p[3][1]]
    }

    fn find_extrema(p: &P, k: usize) -> Vec<f64> {
        let (a, b, c, d) = (p[0][k], p[1][k], p[2][k], p[3][k]);
        quad_roots_valid_t(d - a + 3.0 * (b - c), 2.0 * (a - b - b + c), b - a)
    }

    fn find_inflections(p: &P) -> Vec<f64> {
        let ax = p[1][0] - p[0][0];
        let ay = p[1][1] - p[0][1];
        let bx = p[2][0] - 2.0 * p[1][0] + p[0][0];
        let by = p[2][1] - 2.0 * p[1][1] + p[0][1];
        let cx = p[3][0] + 3.0 * (p[1][0] - p[2][0]) - p[0][0];
        let cy = p[3][1] + 3.0 * (p[1][1] - p[2][1]) - p[0][1];
        quad_roots_valid_t(bx * cy - by * cx, ax * cy - ay * cx, ax * by - ay * bx)
    }

    fn binary_search(p: &P, min: f64, max: f64, v: f64, k: usize) -> f64 {
        let mut t = (min + max) / 2.0;
        let mut step = (t - min) / 2.0;
        let mut at = point_at_t(p, t);
        let mut dist = at[k] - v;
        // Halving steps reach the f64 resolution long before this.
        for _ in 0..4096 {
            let prior_t = min.max(t - step);
            let less = point_at_t(p, prior_t);
            if approximately_equal_half(less[0], at[0]) && approximately_equal_half(less[1], at[1]) {
                return -1.0;
            }
            let less_dist = less[k] - v;
            let last_step = step;
            step /= 2.0;
            let ok = if dist > 0.0 { dist > less_dist } else { dist < less_dist };
            if ok {
                t = prior_t;
            } else {
                let next_t = t + last_step;
                if next_t > max {
                    return -1.0;
                }
                let more = point_at_t(p, next_t);
                if approximately_equal_half(more[0], at[0]) && approximately_equal_half(more[1], at[1]) {
                    return -1.0;
                }
                let more_dist = more[k] - v;
                let ok = if dist > 0.0 { dist <= more_dist } else { dist >= more_dist };
                if ok {
                    continue;
                }
                t = next_t;
            }
            at = point_at_t(p, t);
            dist = at[k] - v;
            if approximately_equal(at[k], v) {
                return t;
            }
        }
        -1.0
    }

    fn search_roots(p: &P, mut ts: Vec<f64>, v: f64, k: usize) -> Vec<f64> {
        ts.extend(find_inflections(p));
        ts.push(0.0);
        ts.push(1.0);
        ts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut roots = Vec::new();
        for w in ts.windows(2) {
            let (min, max) = (w[0], w[1]);
            if min == max {
                continue;
            }
            let t = binary_search(p, min, max, v, k);
            if t >= 0.0 {
                if roots.len() >= 3 {
                    return Vec::new();
                }
                roots.push(t);
            }
        }
        roots
    }

    /// `horizontal_intersect` (`k` 1) / `vertical_intersect` (`k` 0): the first root.
    pub(super) fn intercept(p: &P, v: f64, k: usize) -> Option<f64> {
        // `coefficients`.
        let (pa, pb, pc, pd) = (p[0][k], p[1][k], p[2][k], p[3][k]);
        let mut a = pd;
        let mut b = pc * 3.0;
        let mut c = pb * 3.0;
        let d = pa;
        a -= d - c + b;
        b += 3.0 * d - 2.0 * c;
        c -= 3.0 * d;
        let mut roots = roots_valid_t(a, b, c, d - v);
        for &t in &roots {
            if !approximately_equal(point_at_t(p, t)[k], v) {
                roots = search_roots(p, find_extrema(p, k), v, k);
                break;
            }
        }
        roots.first().copied()
    }
}

/// `Cubic64::chop_at`, back in f32.
fn chop_cubic64(p: &[[f64; 2]; 4], t: f64) -> [Pt; 7] {
    let mut dst = [[0.0f32; 2]; 7];
    for k in 0..2 {
        let (a, b, c, d) = (p[0][k], p[1][k], p[2][k], p[3][k]);
        let v = if t == 0.5 {
            [a, (a + b) / 2.0, (a + 2.0 * b + c) / 4.0, (a + 3.0 * (b + c) + d) / 8.0, (b + 2.0 * c + d) / 4.0, (c + d) / 2.0, d]
        } else {
            let i = |x: f64, y: f64| x + (y - x) * t;
            let (ab, bc, cd) = (i(a, b), i(b, c), i(c, d));
            let (abc, bcd) = (i(ab, bc), i(bc, cd));
            [a, ab, abc, i(abc, bcd), bcd, cd, d]
        };
        for (o, x) in dst.iter_mut().zip(v) {
            o[k] = x as f32;
        }
    }
    dst
}

fn mono_cubic_closest_t(src: [f32; 4], x: f32) -> f32 {
    let mut t = 0.5f32;
    let mut best_t = t;
    let mut step = 0.25f32;
    let d = src[0];
    let a = src[3] + 3.0 * (src[1] - src[2]) - d;
    let b = 3.0 * (src[2] - src[1] - src[1] + d);
    let c = 3.0 * (src[1] - d);
    let x = x - d;
    let mut closest = f32::MAX;
    loop {
        let loc = ((a * t + b) * t + c) * t;
        let dist = (loc - x).abs();
        if closest > dist {
            closest = dist;
            best_t = t;
        }
        let last_t = t;
        t += if loc < x { step } else { -step };
        step *= 0.5;
        if !(closest > 0.25 && last_t != t) {
            break;
        }
    }
    best_t
}

// ---------------------------------------------------------------- `fill_coverage`'s huge paths

/// `effectcraft_path::clip_to_rect`: `path` (device space) flattened and clipped to a little more
/// than the `w × h` mask, one closed polygon per subpath.
fn clip_to_rect(path: &BezPath, w: u32, h: u32) -> BezPath {
    let (x0, y0, x1, y1) = (-2.0, -2.0, f64::from(w) + 2.0, f64::from(h) + 2.0);
    let mut polys: Vec<Vec<kurbo::Point>> = vec![];
    kurbo::flatten(path, 0.05, |el| match el {
        PathEl::MoveTo(p) => polys.push(vec![p]),
        PathEl::LineTo(p) => match polys.last_mut() {
            Some(poly) => poly.push(p),
            None => polys.push(vec![p]),
        },
        _ => {}
    });
    // Left, right, top, bottom: (axis, limit, keep >= limit).
    let sides = [(0usize, x0, true), (0, x1, false), (1, y0, true), (1, y1, false)];
    let mut out = BezPath::new();
    for poly in polys {
        let mut pts = poly;
        for (axis, lim, ge) in sides {
            let Some(&last) = pts.last() else { break };
            let inside = |p: kurbo::Point| {
                let v = if axis == 0 { p.x } else { p.y };
                if ge { v >= lim } else { v <= lim }
            };
            let cross = |a: kurbo::Point, b: kurbo::Point| {
                if axis == 0 {
                    let t = if b.x == a.x { 0.0 } else { (lim - a.x) / (b.x - a.x) };
                    kurbo::Point::new(lim, a.y + (b.y - a.y) * t)
                } else {
                    let t = if b.y == a.y { 0.0 } else { (lim - a.y) / (b.y - a.y) };
                    kurbo::Point::new(a.x + (b.x - a.x) * t, lim)
                }
            };
            let mut next = Vec::with_capacity(pts.len() + 2);
            let mut a = last;
            for &b in &pts {
                match (inside(a), inside(b)) {
                    (true, true) => next.push(b),
                    (true, false) => next.push(cross(a, b)),
                    (false, true) => {
                        next.push(cross(a, b));
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use effectcraft_geom::vec2;
    use effectcraft_path::FillRule;
    use kurbo::Shape;

    /// Coverage from scan edges, as the GPU kernels compute it (`shapes.wgsl` `sh_row`).
    pub(crate) fn coverage(edges: &[ScanEdge], w: u32, h: u32, rule: FillRule) -> Vec<f32> {
        let (w, h) = (w as usize, h as usize);
        let mut d = vec![0i32; w * 4 * h * 4];
        for e in edges {
            let (n, wind) = (e[1] >> 1, if e[1] & 1 == 1 { -1 } else { 1 });
            for k in 0..n {
                let sy = e[0] + k;
                if sy < 0 || sy as usize >= h * 4 {
                    continue;
                }
                let x = e[2].wrapping_add(e[3].wrapping_mul(k));
                let sx = (x.wrapping_add(0x8000) >> 16).max(0) as usize;
                if sx < w * 4 {
                    d[sy as usize * w * 4 + sx] += wind;
                }
            }
        }
        let mut out = vec![0.0; w * h];
        for y in 0..h {
            let mut alpha = vec![0u32; w];
            for j in 0..4 {
                let row = &d[(y * 4 + j) * w * 4..(y * 4 + j + 1) * w * 4];
                let mut wv = 0;
                for (a, samples) in alpha.iter_mut().zip(row.chunks(4)) {
                    let mut cnt = 0;
                    for v in samples {
                        wv += v;
                        let on = if rule == FillRule::EvenOdd { wv & 1 != 0 } else { wv != 0 };
                        cnt += u32::from(on);
                    }
                    *a += if cnt == 4 { 64 - u32::from(j == 3) } else { 16 * cnt };
                }
            }
            for (o, a) in out[y * w..(y + 1) * w].iter_mut().zip(&alpha) {
                *o = (*a).min(255) as f32 / 255.0;
            }
        }
        out
    }

    fn check(paths: &[BezPath], m: &Mat3, w: u32, h: u32, rule: FillRule) {
        let cpu = effectcraft_path::fill_coverage(paths, m, w, h, rule);
        let Scan::Edges(edges) = scan_edges(paths, m, w, h) else { panic!("edges expected") };
        let ours = coverage(&edges, w, h, rule);
        let bad: Vec<(usize, f32, f32)> = cpu.data.iter().zip(&ours).enumerate().filter(|(_, (a, b))| a != b).map(|(i, (a, b))| (i, *a, *b)).collect();
        // Exact but for a pixel whose four samples on its last sub-scanline come from two spans
        // (tiny-skia then sums 32 + 32, not 63): one level, where a path crosses itself.
        let one_level = bad.iter().all(|(_, a, b)| (a - b).abs() <= 1.0 / 255.0 + 1e-6);
        assert!(one_level && bad.len() * 200 <= cpu.data.len(), "{} of {} pixels differ, first {:?}", bad.len(), cpu.data.len(), &bad[..bad.len().min(8)]);
    }

    #[test]
    fn scan_edges_give_tiny_skias_coverage_exactly() {
        let shapes = [
            kurbo::Rect::new(3.3, 2.7, 40.6, 21.1).to_path(0.1),
            kurbo::Ellipse::new((25.0, 16.0), (19.3, 11.7), 0.4).to_path(0.1),
            kurbo::RoundedRect::new(2.2, 3.4, 45.9, 28.1, 7.5).to_path(0.1),
        ];
        let mut star = BezPath::new();
        for i in 0..11 {
            let a = i as f64 * std::f64::consts::TAU * 2.0 / 5.0;
            let p = (24.0 + 13.0 * a.cos(), 15.0 + 13.0 * a.sin());
            if i == 0 { star.move_to(p) } else { star.line_to(p) }
        }
        star.close_path();
        let mut quad = BezPath::new();
        quad.move_to((4.0, 4.0));
        quad.quad_to((40.0, -10.0), (44.0, 26.0));
        quad.quad_to((10.0, 40.0), (4.0, 4.0));
        let ms = [Mat3::IDENTITY, Mat3::translate(vec2(0.37, 0.61)) * Mat3::rotate_deg(3.0) * Mat3::scale(vec2(0.93, 1.07))];
        for m in &ms {
            for p in shapes.iter().chain([&star, &quad]) {
                for rule in [FillRule::NonZero, FillRule::EvenOdd] {
                    check(std::slice::from_ref(p), m, 50, 32, rule);
                }
            }
        }
        // A stroke outline (joins, caps, dashes) is filled the same way.
        let mut st = effectcraft_path::StrokeStyle { width: 3.5, ..Default::default() };
        st.dash = Some((vec![6.0, 3.0], 1.5));
        let o = effectcraft_path::stroke_outline(std::slice::from_ref(&star), &st, 1.0).unwrap();
        check(&[o], &Mat3::IDENTITY, 50, 32, FillRule::NonZero);
    }

    #[test]
    fn paths_reaching_out_of_the_mask_are_clipped_as_tiny_skia_clips_them() {
        // Every side, lines, quads and cubics, both rules; then far out (`fill_coverage`'s
        // flatten-and-clip path) and rotated past the corners.
        let mut quad = BezPath::new();
        quad.move_to((-12.0, 5.0));
        quad.quad_to((30.0, -25.0), (62.0, 20.0));
        quad.quad_to((20.0, 50.0), (-12.0, 5.0));
        let shapes = [
            kurbo::Rect::new(-3.0, 2.0, 20.0, 10.0).to_path(0.1),
            kurbo::Rect::new(10.3, -4.6, 61.2, 40.7).to_path(0.1),
            kurbo::Ellipse::new((25.0, 16.0), (31.3, 9.7), 0.4).to_path(0.1),
            kurbo::Ellipse::new((2.0, 30.0), (12.5, 8.2), 1.1).to_path(0.1),
            kurbo::RoundedRect::new(-8.2, -6.4, 45.9, 38.1, 9.5).to_path(0.1),
            quad,
        ];
        let ms = [
            Mat3::IDENTITY,
            Mat3::translate(vec2(25.0, 16.0)) * Mat3::rotate_deg(33.0) * Mat3::scale(vec2(1.4, 1.2)) * Mat3::translate(vec2(-25.0, -16.0)),
            Mat3::translate(vec2(25.0, 16.0)) * Mat3::rotate_deg(-71.0) * Mat3::scale(vec2(300.0, 250.0)) * Mat3::translate(vec2(-25.0, -16.0)),
        ];
        for m in &ms {
            for p in &shapes {
                for rule in [FillRule::NonZero, FillRule::EvenOdd] {
                    check(std::slice::from_ref(p), m, 50, 32, rule);
                }
            }
        }
        // Wholly outside: nothing.
        let r = kurbo::Rect::new(-30.0, 2.0, -20.0, 10.0).to_path(0.1);
        assert_eq!(scan_edges(&[r], &Mat3::IDENTITY, 30, 20), Scan::Empty);
    }

    #[test]
    fn junk_and_degenerate_paths_are_empty() {
        let mut nan = BezPath::new();
        nan.move_to((f64::NAN, 1.0));
        nan.line_to((3.0, 3.0));
        nan.line_to((1.0, 5.0));
        assert_eq!(scan_edges(&[nan], &Mat3::IDENTITY, 30, 20), Scan::Empty);
        let mut line = BezPath::new();
        line.move_to((1.0, 1.0));
        line.line_to((9.0, 1.0));
        assert_eq!(scan_edges(&[line], &Mat3::IDENTITY, 30, 20), Scan::Empty);
        assert_eq!(scan_edges(&[], &Mat3::IDENTITY, 30, 20), Scan::Empty);
    }
}
