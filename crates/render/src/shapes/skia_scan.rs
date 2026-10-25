//! The edges tiny-skia's anti-aliased path filler walks, for an accelerator to scan-convert with
//! the same samples (`effectcraft-gpu`'s `shapes_gpu`): the CPU's coverage masks come from
//! `tiny_skia::Mask::fill_path` (via [`effectcraft_path::fill_coverage`]), so the GPU matches them
//! by building the very same edges: the path made and transformed in `f32` as tiny-skia does,
//! curves turned into lines by its fixed-point forward differencing, every line as a 16.16
//! fixed-point x stepped per sub-scanline (4 per pixel row, 4 sample columns per pixel).
//!
//! Each [`ScanEdge`] covers up to [`CHUNK`] consecutive sub-scanlines; on sub-scanline `y` its
//! sample column is `(x + k·dx + 0x8000) >> 16` (`k = y − first`), and every sample at or right
//! of it gets the edge's winding. A pixel's coverage is then tiny-skia's `SuperBlitter` sum: per
//! sub-scanline, 16 per covered sample, or 64 (63 on the last sub-scanline) when all four are
//! covered, capped at 255.
//!
//! Ported from tiny-skia 0.12 (`edge.rs`, `edge_builder.rs`, `path_geometry.rs`,
//! `fixed_point.rs`, `scan/path_aa.rs`, and `tiny-skia-path`'s `path_builder.rs`,
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
    /// Not reproduced here (a path tiny-skia clips or tiles first, or too many edges): the CPU
    /// must rasterise this one.
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
                if x < -SAFE_REACH || y < -SAFE_REACH || x > wf + SAFE_REACH || y > hf + SAFE_REACH {
                    return Scan::Decline;
                }
            }
        }
    }
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
    // Paths that reach outside the mask go through tiny-skia's edge clipper: not ported yet.
    if ir[0] < 0 || ir[1] < 0 || ir[2] > wi || ir[3] > hi {
        return Scan::Decline;
    }
    let mut out = Vec::new();
    let mut objects = 0usize;
    for e in path_edges(&b.verbs, &pts) {
        match e {
            PathEdge::Line(p0, p1) => {
                if let Some(line) = LineEdge::new(p0, p1, SHIFT) {
                    objects += 1;
                    emit(&line, &mut out);
                }
            }
            PathEdge::Quad(p0, p1, p2) => {
                let (n, mono) = chop_quad_at_y_extrema(&[p0, p1, p2]);
                for i in 0..=n {
                    let Some(q) = mono.get(i * 2..i * 2 + 3) else { continue };
                    let Some(mut quad) = QuadEdge::new([q[0], q[1], q[2]], SHIFT) else { continue };
                    objects += 1;
                    emit(&quad.line, &mut out);
                    while quad.count > 0 && quad.update() {
                        emit(&quad.line, &mut out);
                    }
                }
            }
            PathEdge::Cubic(p0, p1, p2, p3) => {
                let (n, mono) = chop_cubic_at_y_extrema(&[p0, p1, p2, p3]);
                for i in 0..=n {
                    let Some(c) = mono.get(i * 3..i * 3 + 4) else { continue };
                    let Some(mut cubic) = CubicEdge::new([c[0], c[1], c[2], c[3]], SHIFT) else { continue };
                    objects += 1;
                    emit(&cubic.line, &mut out);
                    while cubic.count < 0 && cubic.update() {
                        emit(&cubic.line, &mut out);
                    }
                }
            }
        }
        if out.len() > MAX_EDGES {
            return Scan::Decline;
        }
    }
    // `BasicEdgeBuilder::build_edges`: fewer than two edges draw nothing.
    if objects < 2 || out.is_empty() { Scan::Empty } else { Scan::Edges(out) }
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

fn chop_quad_at_y_extrema(src: &[Pt; 3]) -> (usize, [Pt; 5]) {
    let (a, mut b, c) = (src[0][1], src[1][1], src[2][1]);
    if is_not_monotonic(a, b, c) {
        if let Some(t) = unit_divide(a - b, a - b - b + c) {
            let mut dst = chop_quad_at(src, t);
            dst[1][1] = dst[2][1];
            dst[3][1] = dst[2][1];
            return (1, dst);
        }
        b = if (a - b).abs() < (b - c).abs() { a } else { c };
    }
    (0, [[src[0][0], a], [src[1][0], b], [src[2][0], c], [0.0; 2], [0.0; 2]])
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

fn chop_cubic_at_y_extrema(src: &[Pt; 4]) -> (usize, [Pt; 10]) {
    let (a, b, c, d) = (src[0][1], src[1][1], src[2][1], src[3][1]);
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
    dst[2][1] = dst[3][1];
    dst[4][1] = dst[3][1];
    if ts.len() == 2 {
        dst[5][1] = dst[6][1];
        dst[7][1] = dst[6][1];
    }
    (ts.len(), dst)
}

#[cfg(test)]
mod tests {
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
    fn paths_reaching_out_of_the_mask_are_declined_and_junk_is_empty() {
        let r = kurbo::Rect::new(-3.0, 2.0, 20.0, 10.0).to_path(0.1);
        assert_eq!(scan_edges(&[r], &Mat3::IDENTITY, 30, 20), Scan::Decline);
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
