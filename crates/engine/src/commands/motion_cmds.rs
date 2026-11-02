//! `comp.motion` / `layer.motion`: how layers move over a time range, as segments (enter, exit,
//! move, hold, scale, rotate, fade-in, fade-out), off-screen intervals, overlap events and one
//! sentence per layer (MCP `get_motion`). Agents can't watch a video; this reads the motion
//! without rendering. The analysis itself is [`crate::motion`].

use effectcraft_project::{Comp, LayerId};
use effectcraft_render::RenderOpts;
use effectcraft_time::Tick;
use serde_json::Value;

use super::layout_cmds::{named_layers, query_comp, renderer};
use super::{CommandSpec, bad, no_layer, resolve_layer};
use crate::{EngineError, Result, Session, motion, query};

/// `start` / `end` / `step` (seconds) → the range to sample: default the whole comp
/// (0..duration) and one frame, or coarser so a layer gets at most 600 samples.
fn range_p(comp: &Comp, p: &Value, cmd: &str) -> Result<(Tick, Tick, Tick)> {
    let secs = |k: &str| -> Result<Option<f64>> {
        match p.get(k).filter(|v| !v.is_null()) {
            None => Ok(None),
            Some(v) => v.as_f64().filter(|x| x.is_finite()).map(Some).ok_or_else(|| bad(cmd, format!("`{k}` must be a number of seconds"))),
        }
    };
    let dur = comp.duration.max(Tick::ZERO);
    let start = secs("start")?.map_or(Tick::ZERO, Tick::from_seconds_f64).clamp(Tick::ZERO, dur);
    let end = secs("end")?.map_or(dur, Tick::from_seconds_f64).clamp(Tick::ZERO, dur);
    if end <= start {
        return Err(bad(
            cmd,
            format!("empty range: `end` ({}s) must be after `start` ({}s), inside the comp's 0–{}s", end.seconds(), start.seconds(), dur.seconds()),
        ));
    }
    let step = match secs("step")? {
        Some(s) if s > 0.0 => Tick::from_seconds_f64(s).max(Tick(1)),
        Some(_) => return Err(bad(cmd, "`step` must be more than 0 seconds")),
        None => motion::default_step(end - start, comp.frame_rate.frame_duration()),
    };
    Ok((start, end, step))
}

fn run(s: &Session, p: &Value, cmd: &str, ids: impl FnOnce(&Comp) -> Result<Vec<LayerId>>) -> Result<Value> {
    let cid = query_comp(s, p)?;
    let comp = s.project.comp(cid).ok_or(EngineError::NoComp)?;
    let ids = ids(comp)?;
    let (start, end, step) = range_p(comp, p, cmd)?;
    let r = renderer(s, RenderOpts::default());
    let ctx = r.eval_ctx(cid, start).ok_or(EngineError::NoComp)?;
    Ok(motion::describe(&ctx, &ids, start, end, step))
}

fn comp_motion(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "comp.motion";
    run(s, p, C, |comp| named_layers(comp, p, C))
}

fn layer_motion(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "layer.motion";
    run(s, p, C, |comp| {
        let v = p.get("layer").filter(|v| !v.is_null()).ok_or_else(|| bad(C, "missing `layer` (a layer id, \"#n\" or a name)"))?;
        Ok(vec![resolve_layer(comp, v).ok_or_else(|| bad(C, no_layer(v)))?])
    })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        query!(
            "comp.motion",
            "Motion Summary",
            "{comp?, layers?: [id|name|#n] (default every layer), start? (s, default 0), end? (s, default the comp's duration), step? (s, default one frame or coarser, ≤ 600 samples per layer)} → {comp, compName, start, end, step, truncated, layers: [{layer, name, inPoint, outPoint, segments: [{kind: enter|exit|move|hold|scale|rotate|fade-in|fade-out, start, end, from, to, direction?, edge?}], offscreen: [{start, end}], summary}], overlaps: [{a, b, aName, bName, start, end, maxFraction}]}: how layers move, without rendering",
            comp_motion
        ),
        query!(
            "layer.motion",
            "Layer Motion Summary",
            "{comp?, layer: id|name|#n, start? (s, default 0), end? (s, default the comp's duration), step? (s)} → comp.motion's result for one layer",
            layer_motion
        ),
    ]
}
