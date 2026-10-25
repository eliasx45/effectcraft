//! The graph view of the taste check (`taste.graph`): a property's value graph over a time range
//! with its speed graph under it, as a PNG an agent reads and shows Elias. Keys are marked; each
//! move's speed curve is coloured by its taste verdict (✅ green, 🟡 amber, ❌ red) and labelled
//! with the curve it matches. Labels use a built-in 5 × 7 pixel font (capitals, digits and a few
//! signs), so no font files are needed.

use effectcraft_keyframe::speed;
use effectcraft_project::{Layer, Property};
use effectcraft_time::Tick;
use image::{Rgba, RgbaImage};

use crate::taste::{Judged, Verdict};

/// Smallest and largest image side (px).
pub const MIN_SIDE: u32 = 240;
pub const MAX_SIDE: u32 = 4096;

const BG: [u8; 3] = [0x11, 0x11, 0x11];
const PANEL: [u8; 3] = [0x1b, 0x1b, 0x1b];
const GRID: [u8; 3] = [0x2c, 0x2c, 0x2c];
const TEXT: [u8; 3] = [0xd8, 0xd8, 0xd8];
const DIM: [u8; 3] = [0x8a, 0x8a, 0x8a];
const KEY: [u8; 3] = [0xff, 0xff, 0xff];
/// One colour per value dimension (his 10-10 blues, then amber).
const DIMS: [[u8; 3]; 4] = [[0x46, 0x97, 0xf3], [0xc5, 0xe4, 0xff], [0x23, 0x61, 0x97], [0xff, 0xb0, 0x00]];
const OK: [u8; 3] = [0x3c, 0xcf, 0x91];
const CLOSE: [u8; 3] = [0xff, 0xb0, 0x00];
const OFF: [u8; 3] = [0xff, 0x5a, 0x4e];
const PLAIN: [u8; 3] = [0xc5, 0xe4, 0xff];

/// 5 × 7 glyphs, one byte per row (low 5 bits, bit 4 = left column).
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0, 0, 0, 0, 0, 0x0C, 0x0C],
        ',' => [0, 0, 0, 0, 0x0C, 0x04, 0x08],
        ':' => [0, 0x0C, 0x0C, 0, 0x0C, 0x0C, 0],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        '+' => [0, 0x04, 0x04, 0x1F, 0x04, 0x04, 0],
        '/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '=' => [0, 0, 0x1F, 0, 0x1F, 0, 0],
        '#' => [0x0A, 0x0A, 0x1F, 0x0A, 0x1F, 0x0A, 0x0A],
        '\'' => [0x04, 0x04, 0x08, 0, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 0x1F],
        '!' => [0x04, 0x04, 0x04, 0x04, 0x04, 0, 0x04],
        '?' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0, 0x04],
        '·' | '•' | '*' => [0, 0, 0x04, 0x0E, 0x04, 0, 0],
        '×' => [0, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0],
        '›' | '→' => [0, 0x04, 0x02, 0x1F, 0x02, 0x04, 0],
        _ => [0; 7],
    }
}

struct Canvas {
    img: RgbaImage,
}

impl Canvas {
    fn put(&mut self, x: i64, y: i64, c: [u8; 3], a: f64) {
        let (w, h) = (self.img.width() as i64, self.img.height() as i64);
        if x < 0 || y < 0 || x >= w || y >= h || a <= 0.0 {
            return;
        }
        let Some(p) = self.img.get_pixel_mut_checked(x as u32, y as u32) else { return };
        let a = a.min(1.0);
        for (k, ch) in c.iter().enumerate() {
            if let Some(v) = p.0.get_mut(k) {
                *v = (f64::from(*v) * (1.0 - a) + f64::from(*ch) * a).round() as u8;
            }
        }
    }

    fn rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: [u8; 3]) {
        for yy in y..y.saturating_add(h) {
            for xx in x..x.saturating_add(w) {
                self.put(xx, yy, c, 1.0);
            }
        }
    }

    /// A line `thick` px wide (Wu-style coverage from the distance to the segment).
    fn line(&mut self, a: (f64, f64), b: (f64, f64), thick: f64, c: [u8; 3]) {
        if !(a.0.is_finite() && a.1.is_finite() && b.0.is_finite() && b.1.is_finite()) {
            return;
        }
        let r = thick / 2.0 + 1.0;
        let (x0, x1) = (a.0.min(b.0) - r, a.0.max(b.0) + r);
        let (y0, y1) = (a.1.min(b.1) - r, a.1.max(b.1) + r);
        if (x1 - x0) * (y1 - y0) > 4.0e6 {
            return;
        }
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = (dx * dx + dy * dy).max(1e-12);
        for y in y0.floor() as i64..=y1.ceil() as i64 {
            for x in x0.floor() as i64..=x1.ceil() as i64 {
                let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                let t = (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0);
                let (qx, qy) = (a.0 + t * dx - px, a.1 + t * dy - py);
                let d = (qx * qx + qy * qy).sqrt();
                self.put(x, y, c, (thick / 2.0 + 0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    fn dot(&mut self, at: (f64, f64), r: f64, c: [u8; 3]) {
        for y in (at.1 - r - 1.0).floor() as i64..=(at.1 + r + 1.0).ceil() as i64 {
            for x in (at.0 - r - 1.0).floor() as i64..=(at.0 + r + 1.0).ceil() as i64 {
                let d = ((x as f64 + 0.5 - at.0).powi(2) + (y as f64 + 0.5 - at.1).powi(2)).sqrt();
                self.put(x, y, c, (r + 0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// Text at `scale` px per font pixel; returns its width.
    fn text(&mut self, x: i64, y: i64, s: &str, scale: i64, c: [u8; 3]) -> i64 {
        let mut cx = x;
        for ch in s.chars().take(200) {
            let g = glyph(ch);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5i64 {
                    if bits & (0x10 >> col) != 0 {
                        self.rect(cx + col * scale, y + row as i64 * scale, scale, scale, c);
                    }
                }
            }
            cx += 6 * scale;
        }
        cx - x
    }
}

fn text_width(s: &str, scale: i64) -> i64 {
    s.chars().take(200).count() as i64 * 6 * scale
}

fn verdict_colour(v: Option<Verdict>) -> [u8; 3] {
    match v {
        Some(Verdict::Approved) => OK,
        Some(Verdict::Close) => CLOSE,
        Some(Verdict::Off) => OFF,
        None => PLAIN,
    }
}

fn short(x: f64) -> String {
    let a = x.abs();
    if a >= 1000.0 {
        format!("{x:.0}")
    } else if a >= 10.0 {
        format!("{x:.1}")
    } else {
        format!("{x:.2}")
    }
}

/// Draws `prop` of `l` from comp second `t0` to `t1` (`fps` for frame labels), with the judged
/// moves of that property (`moves`), on a `w` × `h` image.
#[allow(clippy::too_many_arguments)]
pub fn draw(l: &Layer, prop: &Property, path: &str, comp_name: &str, t0: f64, t1: f64, fps: f64, moves: &[&Judged], w: u32, h: u32) -> RgbaImage {
    let (w, h) = (w.clamp(MIN_SIDE, MAX_SIDE), h.clamp(MIN_SIDE, MAX_SIDE));
    let mut cv = Canvas { img: RgbaImage::from_pixel(w, h, Rgba([BG[0], BG[1], BG[2], 255])) };
    let s = if w >= 1100 { 2 } else { 1 };
    let (wf, hf) = (w as f64, h as f64);
    let left = 14.0 * s as f64 + 50.0;
    let right = wf - 16.0;
    let top = 26.0 * s as f64 + 18.0;
    let bottom = hf - 18.0 * s as f64 - 30.0;
    let gap = 26.0 * s as f64;
    let ph = ((bottom - top - gap) / 2.0).max(10.0);
    let (vy0, sy0) = (top, top + ph + gap);
    cv.text(14, 10, &format!("{comp_name} › {} › {path}", l.name), s, TEXT);
    let span = (t1 - t0).max(1e-6);
    let samples = ((right - left) as usize).clamp(16, 4000);
    let at = |i: usize| t0 + span * i as f64 / samples as f64;
    let x_of = |t: f64| left + (t - t0) / span * (right - left);
    let layer_t = |ct: f64| l.layer_time(Tick::from_seconds_f64(ct));
    // Values (each dimension), and speeds.
    let mut vals: Vec<Vec<f64>> = vec![];
    let mut speeds: Vec<f64> = vec![];
    for i in 0..=samples {
        let lt = layer_t(at(i));
        vals.push(prop.value_at(lt).components().into_iter().take(DIMS.len()).collect());
        let v = speed(&prop.keys, lt, prop.spatial) * 100.0 / l.stretch.abs().max(1e-6);
        speeds.push(if v.is_finite() { v.abs() } else { 0.0 });
    }
    let dims = vals.iter().map(Vec::len).max().unwrap_or(0);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for v in vals.iter().flatten() {
        if v.is_finite() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    if lo > hi {
        (lo, hi) = (0.0, 1.0);
    }
    if (hi - lo).abs() < 1e-9 {
        (lo, hi) = (lo - 1.0, hi + 1.0);
    }
    let pad = (hi - lo) * 0.08;
    let (lo, hi) = (lo - pad, hi + pad);
    let smax = speeds.iter().copied().fold(0.0, f64::max).max(1e-9) * 1.1;
    for (y0, label, top_v, low_v) in [(vy0, "VALUE", hi, lo), (sy0, "SPEED /S", smax, 0.0)] {
        cv.rect(left as i64, y0 as i64, (right - left) as i64, ph as i64, PANEL);
        for k in 1..4 {
            let y = y0 + ph * k as f64 / 4.0;
            cv.line((left, y), (right, y), 1.0, GRID);
        }
        cv.text(left as i64 + 6, y0 as i64 + 5, label, s, DIM);
        let tv = short(top_v);
        cv.text(left as i64 - text_width(&tv, s) - 6, y0 as i64, &tv, s, DIM);
        let lv = short(low_v);
        cv.text(left as i64 - text_width(&lv, s) - 6, (y0 + ph) as i64 - 7 * s, &lv, s, DIM);
    }
    let vy = |v: f64| vy0 + ph - (v - lo) / (hi - lo) * ph;
    let sy = |v: f64| sy0 + ph - v / smax * ph;
    for d in 0..dims {
        let c = DIMS.get(d).copied().unwrap_or(PLAIN);
        for i in 0..samples {
            let (Some(a), Some(b)) = (vals.get(i).and_then(|v| v.get(d)), vals.get(i + 1).and_then(|v| v.get(d))) else { continue };
            cv.line((x_of(at(i)), vy(*a)), (x_of(at(i + 1)), vy(*b)), 2.0 * s as f64, c);
        }
    }
    // Speed, coloured by the verdict of the move it belongs to.
    let verdict_at = |t: f64| moves.iter().find(|m| t >= m.t0.min(m.t1) - 1e-9 && t <= m.t0.max(m.t1) + 1e-9).map(|m| m.verdict);
    for i in 0..samples {
        let (Some(a), Some(b)) = (speeds.get(i), speeds.get(i + 1)) else { continue };
        let c = verdict_colour(verdict_at((at(i) + at(i + 1)) / 2.0));
        cv.line((x_of(at(i)), sy(*a)), (x_of(at(i + 1)), sy(*b)), 2.0 * s as f64 + 0.5, c);
    }
    // Keys: a tick through both panels, a dot on the value graph, the frame under it.
    let mut last_label = f64::MIN;
    for k in &prop.keys {
        let ct = l.comp_time(k.time).seconds();
        if ct < t0 - 1e-9 || ct > t1 + 1e-9 {
            continue;
        }
        let x = x_of(ct);
        cv.line((x, vy0), (x, sy0 + ph), 1.0, GRID);
        if let Some(v) = k.value.components().first() {
            cv.dot((x, vy(*v)), 3.0 * s as f64, KEY);
        }
        cv.dot((x, sy(0.0)), 2.5 * s as f64, KEY);
        let f = format!("F{:.0}", ct * fps);
        if x - last_label > text_width(&f, s) as f64 + 6.0 {
            cv.text(x as i64 - text_width(&f, s) / 2, (sy0 + ph) as i64 + 6, &f, s, DIM);
            last_label = x;
        }
    }
    // Each move's verdict and curve, above its speed peak.
    for m in moves {
        let mid = (m.t0 + m.t1) / 2.0;
        let label = match (&m.curve, &m.nearest) {
            (Some(c), _) => format!("OK {c}"),
            (None, Some((_, n, d))) if m.verdict == Verdict::Close => format!("~ {n} {d:.0}%"),
            (None, Some((_, n, _))) => format!("X {}", m.problems.first().map(|p| p.split(':').next().unwrap_or("")).unwrap_or(n.as_str())),
            (None, None) => m.verdict.id().to_string(),
        };
        let lw = text_width(&label, s) as f64;
        let x = (x_of(mid) - lw / 2.0).clamp(left + 2.0, (right - lw - 2.0).max(left + 2.0));
        cv.text(x as i64, sy0 as i64 + 5 + 9 * s, &label, s, verdict_colour(Some(m.verdict)));
    }
    let foot = format!("{:.2}S - {:.2}S  ({:.0} FRAMES)   GREEN = APPROVED  AMBER = CLOSE  RED = OFF", t0, t1, span * fps);
    cv.text(14, h as i64 - 10 * s - 4, &foot, s, DIM);
    cv.img
}
