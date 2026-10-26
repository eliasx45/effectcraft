//! Bending one motion-path segment by its two keys' spatial tangents (no key added).

use super::{Interp, Keyframe, SpatialSeg, spatial_tangents, v3};

/// Control points of the spatial segment `i` → `i + 1`.
fn control_points(keys: &[Keyframe], i: usize) -> Option<[[f64; 3]; 4]> {
    let (a, b) = (keys.get(i)?, keys.get(i.checked_add(1)?)?);
    let (p0, p3) = (v3(&a.value), v3(&b.value));
    let (_, out) = spatial_tangents(keys, i);
    let (inn, _) = spatial_tangents(keys, i + 1);
    Some([p0, std::array::from_fn(|d| p0[d] + out[d]), std::array::from_fn(|d| p3[d] + inn[d]), p3])
}

/// The Bezier parameter (0–1) of the point of segment `i` → `i + 1` nearest `point` (x/y; the
/// motion path is drawn for 2D layers). `None` without that segment.
pub fn spatial_segment_param(keys: &[Keyframe], i: usize, point: [f64; 2]) -> Option<f64> {
    let p = control_points(keys, i)?;
    let dist = |u: f64| {
        let q = SpatialSeg::point(&p, u);
        (q[0] - point[0]).powi(2) + (q[1] - point[1]).powi(2)
    };
    const N: usize = 128;
    let best = (0..=N).map(|k| k as f64 / N as f64).min_by(|a, b| dist(*a).total_cmp(&dist(*b)))?;
    // Refine within one sample step on each side (ternary search).
    let (mut lo, mut hi) = ((best - 1.0 / N as f64).max(0.0), (best + 1.0 / N as f64).min(1.0));
    for _ in 0..40 {
        let (m1, m2) = (lo + (hi - lo) / 3.0, hi - (hi - lo) / 3.0);
        if dist(m1) < dist(m2) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    let u = (lo + hi) / 2.0;
    u.is_finite().then_some(u)
}

/// Bend the spatial segment `i` → `i + 1` so its point at parameter `u` moves by `delta`, the way
/// dragging a segment of a Bezier path bends it: only key `i`'s out tangent and key `i + 1`'s in
/// tangent change (the end nearer the pointer more). Times, values and temporal ease are kept and
/// no key is added. A smooth key (auto or continuous Bezier) turns its other handle to stay
/// opposite, keeping that handle's length; a corner keeps it. Both keys stop being auto Bezier.
pub fn bend_spatial_segment(keys: &[Keyframe], i: usize, u: f64, delta: [f64; 3]) -> Result<Vec<Keyframe>, &'static str> {
    let j = i.checked_add(1).ok_or("no motion-path segment there")?;
    let (a, b) = (keys.get(i).ok_or("no motion-path segment there")?, keys.get(j).ok_or("no motion-path segment there")?);
    if a.out_interp == Interp::Hold {
        return Err("A Hold keyframe jumps: there is no path to bend");
    }
    if !u.is_finite() || !delta.iter().all(|v| v.is_finite()) {
        return Err("Bend needs a finite point and offset");
    }
    // Near an end the other handle can't move the point (the weights blow up): keep away.
    let u = u.clamp(0.05, 0.95);
    // Share the move between the two handles (Inkscape's weighting: the near end does it all
    // within a sixth of the segment, a smooth hand-over in between), then scale each share so the
    // point at `u` moves exactly by `delta`.
    let w = if u <= 1.0 / 6.0 {
        0.0
    } else if u <= 0.5 {
        ((6.0 * u - 1.0) / 2.0).powi(3) / 2.0
    } else if u <= 5.0 / 6.0 {
        (1.0 - ((6.0 * (1.0 - u) - 1.0) / 2.0).powi(3)) / 2.0 + 0.5
    } else {
        1.0
    };
    let k0 = (1.0 - w) / (3.0 * u * (1.0 - u) * (1.0 - u));
    let k1 = w / (3.0 * u * u * (1.0 - u));
    let (a_in, a_out) = spatial_tangents(keys, i);
    let (b_in, b_out) = spatial_tangents(keys, j);
    let new_out: [f64; 3] = std::array::from_fn(|d| a_out[d] + delta[d] * k0);
    let new_in: [f64; 3] = std::array::from_fn(|d| b_in[d] + delta[d] * k1);
    if !new_out.iter().chain(&new_in).all(|v| v.is_finite()) {
        return Err("Bend needs a finite point and offset");
    }
    let mut out = keys.to_vec();
    // `other`: the handle on the far side of the key, turned opposite `moved` when smooth.
    let follow = |k: &Keyframe, moved: [f64; 3], other: [f64; 3]| -> [f64; 3] {
        let smooth = k.spatial_auto || k.spatial_continuous;
        let len = |v: [f64; 3]| v.iter().map(|x| x * x).sum::<f64>().sqrt();
        let (lm, lo) = (len(moved), len(other));
        if smooth && lm > 1e-9 && lo > 1e-9 { moved.map(|v| -v * lo / lm) } else { other }
    };
    if let Some(k) = out.get_mut(i) {
        k.spatial_in = follow(a, new_out, a_in);
        k.spatial_out = new_out;
        k.spatial_continuous = a.spatial_auto || a.spatial_continuous;
        k.spatial_auto = false;
    }
    if let Some(k) = out.get_mut(j) {
        k.spatial_out = follow(b, new_in, b_out);
        k.spatial_in = new_in;
        k.spatial_continuous = b.spatial_auto || b.spatial_continuous;
        k.spatial_auto = false;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Value, evaluate};
    use effectcraft_time::Tick;

    fn two_keys(auto: bool) -> Vec<Keyframe> {
        let mut a = Keyframe::new(Tick::from_seconds_f64(4.77), Value::Vec2([100.0, 100.0]));
        let mut b = Keyframe::new(Tick::from_seconds_f64(5.43), Value::Vec2([500.0, 100.0]));
        a.spatial_auto = auto;
        b.spatial_auto = auto;
        vec![a, b]
    }

    /// Dragging the middle of a straight two-key path pulls it through the pointer: the point
    /// under the press lands where the pointer went, only the two facing tangents change.
    #[test]
    fn bend_moves_the_pressed_point_and_only_the_tangents() {
        for auto in [true, false] {
            let keys = two_keys(auto);
            let u = spatial_segment_param(&keys, 0, [300.0, 103.0]).unwrap_or(0.0);
            assert!((u - 0.5).abs() < 1e-6, "u = {u}");
            let bent = bend_spatial_segment(&keys, 0, u, [0.0, -80.0, 0.0]).unwrap_or_default();
            assert_eq!(bent.len(), 2);
            let p = control_points(&bent, 0).unwrap_or_default();
            let q = SpatialSeg::point(&p, u);
            assert!((q[0] - 300.0).abs() < 1e-6 && (q[1] - 20.0).abs() < 1e-6, "{q:?}");
            for (k, o) in bent.iter().zip(&keys) {
                assert_eq!(
                    (k.time, &k.value, &k.in_ease, &k.out_ease, k.in_interp, k.out_interp),
                    (o.time, &o.value, &o.in_ease, &o.out_ease, o.in_interp, o.out_interp)
                );
                assert!(!k.spatial_auto);
            }
            assert!(bent[0].spatial_out[1] < 0.0 && bent[1].spatial_in[1] < 0.0, "both handles pulled up");
            // The curve really bends: halfway through the move it is off the straight line.
            let mid = evaluate(&bent, Tick::from_seconds_f64(5.1), true).map(|v| v.as_vec3()).unwrap_or_default();
            assert!(mid[1] < 90.0, "{mid:?}");
        }
    }

    #[test]
    fn bend_refuses_holds_and_missing_segments() {
        let mut keys = two_keys(true);
        assert!(bend_spatial_segment(&keys, 1, 0.5, [0.0, 10.0, 0.0]).is_err());
        keys[0].out_interp = Interp::Hold;
        assert!(bend_spatial_segment(&keys, 0, 0.5, [0.0, 10.0, 0.0]).is_err());
        assert!(spatial_segment_param(&keys, 5, [0.0, 0.0]).is_none());
    }
}
