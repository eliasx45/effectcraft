//! `render.contactSheet`: several frames of a comp (or item, or solo layer) on one sheet, each cell
//! labelled with its time and frame number, so one image shows how an animation moves. Optionally
//! also writes the frames as an animated GIF.

use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use super::overlay::{paint, text_paths};
use super::{Target, deliver_png, encode_png, present, request, seconds_p, target_frame, uint_p};
use crate::commands::{b_p, bad, str_p};
use crate::{EngineError, Result, Session};

/// Most cells one sheet holds; larger `count`s and longer `times` lists are capped to it.
pub const MAX_CELLS: usize = 64;
const DEFAULT_COUNT: u64 = 6;
/// Default and limits of the sheet's longest side.
const DEFAULT_SIDE: u32 = 1600;
const MIN_SIDE: u32 = 64;
const MAX_SIDE: u32 = 8192;
const GAP: u32 = 4;
const LABEL_H: u32 = 18;
const LABEL_PX: f64 = 11.0;
const CMD: &str = "render.contactSheet";

/// The times to show (and how many were asked for): `times`, else `count` frames spread evenly
/// from `start` to `end` (default the comp's work area, or the item's duration).
fn sheet_times(s: &Session, p: &Value, target: Target, rate: FrameRate) -> Result<(Vec<Tick>, usize)> {
    if let Some(v) = present(p, "times") {
        let a = v.as_array().ok_or_else(|| bad(CMD, "`times` must be a list of seconds"))?;
        if a.is_empty() {
            return Err(bad(CMD, "`times` is empty"));
        }
        let mut out = Vec::with_capacity(a.len().min(MAX_CELLS));
        for t in a.iter().take(MAX_CELLS) {
            let secs = t.as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad(CMD, format!("`times` holds {t}, not a number of seconds")))?;
            out.push(Tick::from_seconds_f64(secs));
        }
        return Ok((out, a.len()));
    }
    // Comp frame numbers: a list, or every `every`th frame from `from` to `to` (both included).
    if let Some(v) = present(p, "frames") {
        let frame = |x: &Value| x.as_i64().ok_or_else(|| bad(CMD, format!("`frames` holds {x}, not a whole frame number")));
        let list: Vec<i64> = match v {
            Value::Array(a) if !a.is_empty() => a.iter().map(frame).collect::<Result<_>>()?,
            Value::Object(o) => {
                let (from, to) = (o.get("from").map(frame).transpose()?, o.get("to").map(frame).transpose()?);
                let (Some(from), Some(to)) = (from, to) else { return Err(bad(CMD, "`frames` range needs `from` and `to` (comp frames)")) };
                let every = o.get("every").map(frame).transpose()?.unwrap_or(1).max(1);
                if to < from {
                    return Err(bad(CMD, format!("`frames.to` ({to}) is before `frames.from` ({from})")));
                }
                let n = (to.saturating_sub(from) / every).saturating_add(1);
                (0..n.min(MAX_CELLS as i64 + 1)).map(|i| from.saturating_add(i.saturating_mul(every))).collect()
            }
            _ => return Err(bad(CMD, "`frames` must be a non-empty list of comp frame numbers or {from, to, every?}")),
        };
        let n = list.len();
        return Ok((list.into_iter().take(MAX_CELLS).map(|f| rate.tick_of(f)).collect(), n));
    }
    let requested = uint_p(p, &["count"], u64::MAX, CMD)?.unwrap_or(DEFAULT_COUNT);
    if requested == 0 {
        return Err(bad(CMD, "`count` must be at least 1"));
    }
    let (start, end) = match target {
        Target::Comp(cid) => s.project.comp(cid).map(|c| c.work_area).ok_or(EngineError::NoComp)?,
        Target::Item(id) => {
            let d = match s.project.item(id).map(|i| &i.kind) {
                Some(effectcraft_project::ItemKind::Footage(f)) => f.duration,
                _ => Tick::ZERO,
            };
            (Tick::ZERO, d.max(rate.frame_duration()))
        }
    };
    let start = seconds_p(p, "start", CMD)?.map(Tick::from_seconds_f64).unwrap_or(start);
    let end = seconds_p(p, "end", CMD)?.map(Tick::from_seconds_f64).unwrap_or(end);
    if end < start {
        return Err(bad(CMD, format!("`end` ({}s) is before `start` ({}s)", end.seconds(), start.seconds())));
    }
    // Whole frames from the one at `start` to the last one starting before `end`.
    let f0 = rate.frame_at(start);
    let f1 = if end > start { rate.frame_at(Tick(end.0.saturating_sub(1))).max(f0) } else { f0 };
    let span = f1.saturating_sub(f0);
    let n = (requested.min(MAX_CELLS as u64) as i64).min(span.saturating_add(1)).max(1);
    let mut frames: Vec<i64> = (0..n).map(|i| if n == 1 { f0 } else { f0.saturating_add((i as f64 * span as f64 / (n - 1) as f64).round() as i64) }).collect();
    frames.dedup();
    Ok((frames.into_iter().map(|f| rate.tick_of(f)).collect(), requested.min(usize::MAX as u64) as usize))
}

/// Columns for `n` cells of aspect `a` (width / height) with label strips `label` × the cell
/// height: the sheet closest to 16:10.
fn auto_cols(n: usize, a: f64, label: f64) -> usize {
    let target = 1.6_f64.ln();
    let d = |c: usize| {
        let rows = n.div_ceil(c) as f64;
        (((c as f64 * a) / (rows * (1.0 + label))).ln() - target).abs()
    };
    (1..=n.max(1)).min_by(|&x, &y| d(x).total_cmp(&d(y))).unwrap_or(1)
}

/// Cell size for `cols` × `rows` cells of a `fw` × `fh` frame on a sheet whose longest side is at
/// most `side`, never larger than the frame itself.
fn cell_size(cols: u32, rows: u32, fw: f64, fh: f64, side: u32, label_h: u32) -> (u32, u32) {
    let a = fw / fh.max(1.0);
    let side = side as f64;
    let mut cw = (side - ((cols + 1) * GAP) as f64) / cols as f64;
    let mut ch = cw / a;
    let total_h = rows as f64 * (ch + label_h as f64) + ((rows + 1) * GAP) as f64;
    if total_h > side {
        ch = (side - ((rows + 1) * GAP) as f64 - (rows * label_h) as f64) / rows as f64;
        cw = ch * a;
    }
    if cw > fw || ch > fh {
        cw = fw;
        ch = fh;
    }
    ((cw.floor().max(8.0)) as u32, (ch.floor().max(8.0)) as u32)
}

/// Copy a `vw` × `vh` image into the `cw` × `ch` cell at (`x`, `y`) of the sheet, centred and clipped.
#[allow(clippy::too_many_arguments)]
fn blit(sheet: &mut [u8], sw: u32, x: u32, y: u32, cw: u32, ch: u32, img: &[u8], vw: u32, vh: u32) {
    let (ox, oy) = ((cw as i64 - vw as i64) / 2, (ch as i64 - vh as i64) / 2);
    for row in 0..ch as i64 {
        let sy = row - oy;
        if sy < 0 || sy >= vh as i64 {
            continue;
        }
        for col in 0..cw as i64 {
            let sx = col - ox;
            if sx < 0 || sx >= vw as i64 {
                continue;
            }
            let si = ((sy * vw as i64 + sx) * 4) as usize;
            let di = (((y as i64 + row) * sw as i64 + x as i64 + col) * 4) as usize;
            if let (Some(src), Some(dst)) = (img.get(si..si + 4), sheet.get_mut(di..di + 4)) {
                dst.copy_from_slice(src);
            }
        }
    }
}

/// An animated GIF of the cells (each `w` × `h` RGBA), with delays following the times.
fn encode_gif(cells: &mut [Vec<u8>], times: &[Tick], w: u32, h: u32) -> Result<Vec<u8>> {
    let (w16, h16) = (
        u16::try_from(w).map_err(|_| bad(CMD, "GIF frames are limited to 65535 px"))?,
        u16::try_from(h).map_err(|_| bad(CMD, "GIF frames are limited to 65535 px"))?,
    );
    let err = |e: gif::EncodingError| EngineError::Other(format!("GIF encoding failed: {e}"));
    let mut out = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut out, w16, h16, &[]).map_err(err)?;
        enc.set_repeat(gif::Repeat::Infinite).map_err(err)?;
        let n = cells.len();
        for (i, px) in cells.iter_mut().enumerate() {
            if px.len() != w as usize * h as usize * 4 {
                return Err(EngineError::Other("GIF frame has the wrong size".into()));
            }
            // Delay (1/100 s) until the next sampled time; the last frame holds as long as the one before.
            let gap = |a: usize, b: usize| times.get(b).zip(times.get(a)).map(|(tb, ta)| (tb.seconds() - ta.seconds()).abs());
            let secs = if i + 1 < n {
                gap(i, i + 1)
            } else if i > 0 {
                gap(i - 1, i)
            } else {
                None
            };
            let delay = secs.map(|s| (s * 100.0).round()).filter(|d| d.is_finite()).unwrap_or(50.0).clamp(2.0, 65535.0) as u16;
            let mut f = gif::Frame::from_rgba_speed(w16, h16, px, 10);
            f.delay = delay;
            f.dispose = gif::DisposalMethod::Background;
            enc.write_frame(&f).map_err(err)?;
        }
    }
    Ok(out)
}

/// `render.contactSheet`.
pub fn contact_sheet_reply(s: &Session, p: &Value) -> Result<Value> {
    let mut req = request(s, p, CMD)?;
    let (fw, fh, rate) = target_frame(s, req.target).ok_or(EngineError::NoComp)?;
    let (times, requested) = sheet_times(s, p, req.target, rate)?;
    let n = times.len().max(1);
    let labels = b_p(p, "labels").unwrap_or(true);
    let label_h = if labels { LABEL_H } else { 0 };
    let (rw, rh) = req.region.map_or((fw as f64, fh as f64), |r| (r[2], r[3]));
    let side = match uint_p(p, &["max_side"], MAX_SIDE as u64, CMD)? {
        None => DEFAULT_SIDE,
        Some(0) => MAX_SIDE,
        Some(v) => (v as u32).clamp(MIN_SIDE, MAX_SIDE),
    };
    let cols = match uint_p(p, &["cols"], MAX_CELLS as u64, CMD)? {
        Some(0) => return Err(bad(CMD, "`cols` must be at least 1")),
        Some(c) => (c as usize).min(n),
        None => auto_cols(n, rw / rh.max(1.0), if labels { 0.12 } else { 0.0 }),
    }
    .max(1);
    let rows = n.div_ceil(cols);
    let (cols32, rows32) = (cols as u32, rows as u32);
    let (cw, ch) = cell_size(cols32, rows32, rw, rh, side, label_h);
    let sw = cols32 * cw + (cols32 + 1) * GAP;
    let sh = rows32 * (ch + label_h) + (rows32 + 1) * GAP;
    let bg: [u8; 4] = if req.transparent { [0, 0, 0, 0] } else { [24, 24, 24, 255] };
    let mut sheet: Vec<u8> = bg.iter().copied().cycle().take(sw as usize * sh as usize * 4).collect();
    let want_gif = str_p(p, "gif").map(str::to_string);
    let mut cells = vec![];
    let mut first_info = None;
    req.max_side = cw.max(ch);
    for (i, &t) in times.iter().enumerate() {
        req.time = t;
        let v = s.render_view(&req)?;
        let (c, r) = ((i % cols) as u32, (i / cols) as u32);
        let (x, y) = (GAP + c * (cw + GAP), GAP + r * (ch + label_h + GAP));
        blit(&mut sheet, sw, x, y, cw, ch, &v.rgba, v.width, v.height);
        if want_gif.is_some() {
            let mut cell: Vec<u8> = bg.iter().copied().cycle().take(cw as usize * ch as usize * 4).collect();
            blit(&mut cell, cw, 0, 0, cw, ch, &v.rgba, v.width, v.height);
            cells.push(cell);
        }
        if labels {
            let strip = effectcraft_geom::vec2(x as f64, (y + ch) as f64);
            let rect = kurbo::Rect::new(strip.x, strip.y, strip.x + cw as f64, strip.y + label_h as f64);
            paint(&mut sheet, sw, sh, &[kurbo::Shape::to_path(&rect, 0.1)], None, [44, 44, 44, 255]);
            let frame = rate.frame_at(t);
            for text in [format!("{:.2}s  f{frame}", t.seconds()), format!("f{frame}")] {
                let (_, tw, th) = text_paths(&text, LABEL_PX, [0.0, 0.0]);
                if tw > 0.0 && tw + 8.0 <= cw as f64 {
                    let ty = strip.y + ((label_h as f64 - th) / 2.0).max(0.0);
                    let (paths, _, _) = text_paths(&text, LABEL_PX, [strip.x + 4.0, ty]);
                    paint(&mut sheet, sw, sh, &paths, None, [235, 235, 235, 255]);
                    break;
                }
            }
        }
        if first_info.is_none() {
            first_info = Some(v.info);
        }
    }
    let mut out = json!({});
    if let Some(info) = first_info {
        for k in ["comp", "item", "layer", "region", "overlays"] {
            if let Some(v) = info.get(k) {
                out[k] = v.clone();
            }
        }
    }
    out["times"] = json!(times.iter().map(|t| t.seconds()).collect::<Vec<_>>());
    out["frames"] = json!(times.iter().map(|t| rate.frame_at(*t)).collect::<Vec<_>>());
    out["count"] = json!(times.len());
    if requested > times.len() {
        out["requested"] = json!(requested);
        if requested > MAX_CELLS {
            out["note"] = json!(format!("capped at {MAX_CELLS} cells"));
        }
    }
    out["cols"] = json!(cols);
    out["rows"] = json!(rows);
    out["cell"] = json!([cw, ch]);
    out["width"] = json!(sw);
    out["height"] = json!(sh);
    if let Some(path) = want_gif {
        let bytes = encode_gif(&mut cells, &times, cw, ch)?;
        s.services.write_file(&path, &bytes).map_err(|e| EngineError::Other(format!("cannot write {path}: {e}")))?;
        out["gif"] = json!(path);
    }
    let png = encode_png(sw, sh, &sheet)?;
    deliver_png(s, p, &png, &mut out, false, "sheet")?;
    Ok(out)
}
