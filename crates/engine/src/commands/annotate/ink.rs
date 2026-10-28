//! PN2: the Pen's ink: every pointer sample in, a light speed-aware filter, a small stored stroke
//! (Ramer-Douglas-Peucker) and one smooth curve (centripetal Catmull-Rom) that every place drawing a
//! stroke uses: the live viewer, the faint waiting marks and the frame burnt into Ghostex's card.
//! Pure math on `[x, y]` pairs in any one unit (comp pixels, screen points, image pixels).
//!
//! Elias, 28 Oct 2026: "It doesn't follow [my mouse when] I'm moving fast. It's drawing squarish
//! lines and connecting them. Please implement a proper system."
//!
//! CDXC:Editing 2026-10-28 WHY: the viewer took one pointer position per rendered frame and joined
//! them with straight lines, so a fast circle came out as ~10 long segments. Now every
//! `PointerMoved` of the frame is a sample and the drawing is a curve through them, never a polygon.

/// The most points [`smooth`] returns (a stroke never costs more than this to draw).
pub const MAX_SMOOTH_POINTS: usize = 40_000;
/// The most sub-segments between two samples.
const MAX_SUBDIVISIONS: usize = 64;

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

fn finite(p: [f64; 2]) -> bool {
    p[0].is_finite() && p[1].is_finite()
}

/// The live filter: a sample closer than `min_dist` to the last kept point is skipped (hand
/// jitter, duplicates), and the rest are eased toward with a weight that grows with the step, as
/// One Euro does with speed but per sample, since a frame's events share one timestamp: a slow
/// hand is steadied (weight about 0.5), a fast one is followed at once (weight near 1, no lag).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkFilter {
    /// Below this step (the stroke's unit) a sample is skipped.
    pub min_dist: f64,
    /// The step at which the weight is about two thirds: below it the line is steadied.
    pub calm: f64,
    last: Option<[f64; 2]>,
}

impl InkFilter {
    /// `px` is one screen pixel in the stroke's unit (comp pixels per screen point).
    pub fn new(px: f64) -> Self {
        let px = if px.is_finite() && px > 0.0 { px } else { 1.0 };
        InkFilter { min_dist: px, calm: 4.0 * px, last: None }
    }

    /// Feeds one raw sample; the filtered point to keep, or `None` when it is skipped.
    pub fn push(&mut self, p: [f64; 2]) -> Option<[f64; 2]> {
        if !finite(p) {
            return None;
        }
        let Some(last) = self.last else {
            self.last = Some(p);
            return Some(p);
        };
        let d = dist(last, p);
        if d < self.min_dist {
            return None;
        }
        let calm = self.calm.max(1e-9);
        // 0.5 at rest, → 1 when fast.
        let w = 0.5 + 0.5 * (d / (d + calm * 0.5));
        let q = [last[0] + (p[0] - last[0]) * w, last[1] + (p[1] - last[1]) * w];
        self.last = Some(q);
        Some(q)
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Ramer-Douglas-Peucker: drops the points within `tolerance` of the line between the ones kept
/// (iterative, no recursion). The ends stay; non-finite points are dropped first.
pub fn simplify(points: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    let pts: Vec<[f64; 2]> = points.iter().copied().filter(|p| finite(*p)).collect();
    if pts.len() < 3 || tolerance.is_nan() || tolerance <= 0.0 {
        return pts;
    }
    let last = pts.len() - 1;
    let mut keep = vec![false; pts.len()];
    for i in [0, last] {
        if let Some(k) = keep.get_mut(i) {
            *k = true;
        }
    }
    let mut stack = vec![(0usize, last)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (Some(pa), Some(pb)) = (pts.get(a).copied(), pts.get(b).copied()) else {
            continue;
        };
        let mut far = (0.0f64, a);
        for (i, p) in pts.iter().enumerate().take(b).skip(a + 1) {
            let d = segment_distance(*p, pa, pb);
            if d > far.0 {
                far = (d, i);
            }
        }
        if far.0 > tolerance {
            if let Some(k) = keep.get_mut(far.1) {
                *k = true;
            }
            stack.push((a, far.1));
            stack.push((far.1, b));
        }
    }
    pts.into_iter().zip(keep).filter_map(|(p, k)| k.then_some(p)).collect()
}

/// Distance from `p` to the segment `a`–`b`.
pub fn segment_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    if len2 <= f64::EPSILON {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
    dist(p, [a[0] + dx * t, a[1] + dy * t])
}

/// The stroke as one smooth curve: centripetal Catmull-Rom through every point (it never loops or
/// overshoots on sharp turns, unlike the uniform kind), cut into pieces at most `step` long
/// (the caller's unit: ~1.5 screen pixels for drawing). One or two points come back as they are.
pub fn smooth(points: &[[f64; 2]], step: f64) -> Vec<[f64; 2]> {
    let pts: Vec<[f64; 2]> = dedup(points);
    if pts.len() < 3 {
        return pts;
    }
    let step = if step.is_finite() && step > 0.0 { step } else { 1.0 };
    let (Some(&first), Some(&second), Some(&end), Some(&before_end)) = (pts.first(), pts.get(1), pts.last(), pts.get(pts.len() - 2)) else {
        return pts;
    };
    let n = pts.len();
    // Mirrored end points so the curve starts and ends on the stroke's ends.
    let head = [2.0 * first[0] - second[0], 2.0 * first[1] - second[1]];
    let tail = [2.0 * end[0] - before_end[0], 2.0 * end[1] - before_end[1]];
    let at = |i: isize| -> [f64; 2] { if i < 0 { head } else { pts.get(i as usize).copied().unwrap_or(tail) } };
    let mut out = Vec::with_capacity(n.saturating_mul(4).min(MAX_SMOOTH_POINTS));
    out.push(first);
    for i in 0..n - 1 {
        let i = i as isize;
        let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        let len = dist(p1, p2);
        let pieces = ((len / step).ceil() as usize).clamp(1, MAX_SUBDIVISIONS);
        for k in 1..=pieces {
            if out.len() >= MAX_SMOOTH_POINTS {
                return out;
            }
            out.push(catmull_rom(p0, p1, p2, p3, k as f64 / pieces as f64));
        }
    }
    out
}

/// Consecutive duplicates and non-finite points dropped.
fn dedup(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(points.len());
    for p in points.iter().copied().filter(|p| finite(*p)) {
        if out.last().is_none_or(|l| dist(*l, p) > 1e-9) {
            out.push(p);
        }
    }
    out
}

/// The centripetal (alpha 0.5) Catmull-Rom point at `t` in 0..=1 between `p1` and `p2`
/// (Barry-Goldman's pyramid).
fn catmull_rom(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], p3: [f64; 2], t: f64) -> [f64; 2] {
    let knot = |a: [f64; 2], b: [f64; 2]| dist(a, b).sqrt().max(1e-6);
    let t0 = 0.0;
    let t1 = t0 + knot(p0, p1);
    let t2 = t1 + knot(p1, p2);
    let t3 = t2 + knot(p2, p3);
    let t = t1 + (t2 - t1) * t;
    let lerp = |a: [f64; 2], b: [f64; 2], ta: f64, tb: f64| {
        let w = (t - ta) / (tb - ta);
        [a[0] + (b[0] - a[0]) * w, a[1] + (b[1] - a[1]) * w]
    };
    let a1 = lerp(p0, p1, t0, t1);
    let a2 = lerp(p1, p2, t1, t2);
    let a3 = lerp(p2, p3, t2, t3);
    let b1 = lerp(a1, a2, t0, t2);
    let b2 = lerp(a2, a3, t1, t3);
    let c = lerp(b1, b2, t1, t2);
    if finite(c) { c } else { [p1[0] + (p2[0] - p1[0]) * ((t - t1) / (t2 - t1)), p1[1] + (p2[1] - p1[1]) * ((t - t1) / (t2 - t1))] }
}

/// What a finished stroke stores: its filtered samples simplified at `tolerance` (a quarter of a
/// screen pixel in the stroke's unit keeps the curve the same and the data small).
pub fn finish(filtered: &[[f64; 2]], tolerance: f64) -> Vec<[f64; 2]> {
    simplify(filtered, tolerance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(n: usize, r: f64) -> Vec<[f64; 2]> {
        (0..=n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                [r * a.cos(), r * a.sin()]
            })
            .collect()
    }

    #[test]
    fn the_filter_skips_jitter_and_follows_fast_moves() {
        let mut f = InkFilter::new(1.0);
        assert_eq!(f.push([0.0, 0.0]), Some([0.0, 0.0]));
        assert_eq!(f.push([0.3, 0.2]), None, "under a pixel: jitter");
        let fast = f.push([100.0, 0.0]).unwrap_or_default();
        assert!(fast[0] > 97.0, "a fast step is followed almost at once: {fast:?}");
        let mut g = InkFilter::new(1.0);
        g.push([0.0, 0.0]);
        let slow = g.push([2.0, 0.0]).unwrap_or_default();
        assert!(slow[0] > 1.0 && slow[0] < 1.8, "a slow step is steadied: {slow:?}");
        assert_eq!(g.push([f64::NAN, 0.0]), None);
    }

    #[test]
    fn simplify_keeps_the_ends_and_the_corners() {
        let line: Vec<[f64; 2]> = (0..=100).map(|i| [i as f64, 0.0]).collect();
        assert_eq!(simplify(&line, 0.25), vec![[0.0, 0.0], [100.0, 0.0]]);
        let corner = vec![[0.0, 0.0], [5.0, 0.01], [10.0, 0.0], [10.0, 10.0]];
        assert_eq!(simplify(&corner, 0.25), vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]]);
        assert_eq!(simplify(&[[1.0, 1.0]], 0.25), vec![[1.0, 1.0]]);
        assert!(simplify(&[], 0.25).is_empty());
    }

    #[test]
    fn simplify_stays_within_tolerance_of_a_dense_circle() {
        let dense = circle(2000, 200.0);
        let kept = simplify(&dense, 0.25);
        assert!(kept.len() < 200, "{} points", kept.len());
        for p in &dense {
            let d = kept.windows(2).map(|w| segment_distance(*p, w[0], w[1])).fold(f64::MAX, f64::min);
            assert!(d <= 0.25 + 1e-9, "{d}");
        }
    }

    #[test]
    fn a_coarse_circle_comes_out_round_not_a_polygon() {
        // 10 samples of a fast circle: the curve through them stays near the true circle.
        let coarse = circle(10, 100.0);
        let curve = smooth(&coarse, 1.5);
        assert!(curve.len() > 300, "{} points", curve.len());
        let worst = curve.iter().map(|p| (p[0].hypot(p[1]) - 100.0).abs()).fold(0.0, f64::max);
        assert!(worst < 3.0, "off the circle by {worst}");
        // The straight polygon would sag by r(1 - cos(18°)) ≈ 4.9 at each chord's middle.
        assert_eq!(curve.first(), coarse.first());
        let (a, b) = (curve.last().copied().unwrap_or_default(), coarse.last().copied().unwrap_or_default());
        assert!(dist(a, b) < 1e-6);
        for w in curve.windows(2) {
            assert!(dist(w[0], w[1]) <= 1.6, "pieces stay short");
        }
    }

    #[test]
    fn smooth_handles_short_and_bad_input() {
        assert!(smooth(&[], 1.0).is_empty());
        assert_eq!(smooth(&[[1.0, 2.0]], 1.0), vec![[1.0, 2.0]]);
        assert_eq!(smooth(&[[0.0, 0.0], [0.0, 0.0], [3.0, 0.0]], 1.0), vec![[0.0, 0.0], [3.0, 0.0]]);
        let sharp = smooth(&[[0.0, 0.0], [10.0, 0.0], [0.0, 0.1], [10.0, 0.2]], 0.0);
        assert!(sharp.iter().all(|p| finite(*p)));
        let huge: Vec<[f64; 2]> = (0..5000).map(|i| [i as f64 * 1000.0, (i % 2) as f64 * 1000.0]).collect();
        assert!(smooth(&huge, 0.001).len() <= MAX_SMOOTH_POINTS);
    }
}
