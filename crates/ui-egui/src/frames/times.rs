//! The render times of rendered viewer frames, for the Timeline's Render Time pane: the whole
//! frame and each layer (see [`effectcraft_engine::render::LayerTimes`]). They are kept by frame,
//! so a cached frame shows the times from when it was rendered, as in After Effects.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use effectcraft_engine::project::{ItemId, LayerId, Project};
use effectcraft_engine::render::CpuWhy;

use super::FrameKey;

/// One rendered frame's times (ms).
#[derive(Clone, Debug, Default)]
pub struct FrameTimes {
    /// The whole frame.
    pub frame_ms: f64,
    /// Each layer that rendered, by comp and layer: the frame's comp and the comps nested in it.
    pub layers: HashMap<(ItemId, LayerId), f64>,
    /// Rendered by a remote renderer (the cloud, CR5): its times there, not this machine's.
    pub remote: bool,
    /// RI1: the layers that rendered (fully or partly) on the CPU in a GPU frame, and why.
    pub cpu: HashMap<(ItemId, LayerId), CpuWhy>,
    /// RI1: why the whole frame rendered on the CPU; `None`: the GPU composited it.
    pub frame_cpu: Option<CpuWhy>,
}

impl FrameTimes {
    /// The time `layer` of `comp` took on this frame.
    pub fn layer(&self, comp: ItemId, layer: LayerId) -> Option<f64> {
        self.layers.get(&(comp, layer)).copied()
    }

    /// Why `layer` of `comp` rendered (fully or partly) on the CPU on this frame: the whole
    /// frame's reason when it all did, else the layer's own. `None`: on the GPU.
    pub fn layer_cpu(&self, comp: ItemId, layer: LayerId) -> Option<CpuWhy> {
        self.frame_cpu.or_else(|| self.cpu.get(&(comp, layer)).copied())
    }

    /// Why a layer of the nested comp `comp` rendered on the CPU (a precomp layer's tag), the
    /// slowest such layer's.
    pub fn nested_cpu(&self, comp: ItemId) -> Option<CpuWhy> {
        self.cpu.iter().filter(|((c, _), _)| *c == comp).max_by(|a, b| self.ms(*a.0).total_cmp(&self.ms(*b.0))).map(|(_, w)| *w)
    }

    /// The frame and its layers for the control channel (`ui.renderTimes`, RI1): the frame's
    /// `ms`, `backend` (`gpu` | `cpu`), `reason` and whether it was slower than one frame of
    /// `comp`; each layer (slowest first) with its comp, name, `ms`, `backend` and `reason`.
    /// Reasons are in English.
    pub fn to_json(&self, project: &Project, comp: ItemId) -> serde_json::Value {
        use serde_json::json;
        let backend = |cpu: bool| if cpu { "cpu" } else { "gpu" };
        let round = |ms: f64| if ms.is_finite() { (ms * 100.0).round() / 100.0 } else { 0.0 };
        let mut layers: Vec<(&(ItemId, LayerId), &f64)> = self.layers.iter().collect();
        layers.sort_by(|a, b| b.1.total_cmp(a.1));
        let layers: Vec<serde_json::Value> = layers
            .into_iter()
            .map(|(&(c, l), &ms)| {
                let why = self.layer_cpu(c, l);
                json!({
                    "comp": c.0,
                    "layer": l.0,
                    "name": project.comp(c).and_then(|cc| cc.layer(l)).map(|x| x.name.as_str()),
                    "ms": round(ms),
                    "backend": backend(why.is_some()),
                    "reason": why.map(|w| w.text()),
                })
            })
            .collect();
        let frame_ms_budget = project.comp(comp).map(|c| c.frame_rate.as_f64()).filter(|f| f.is_finite() && *f > 0.0).map(|f| 1000.0 / f);
        json!({
            "frame": {
                "comp": comp.0,
                "ms": round(self.frame_ms),
                "backend": backend(self.frame_cpu.is_some()),
                "reason": self.frame_cpu.map(|w| w.text()),
                "frameDurationMs": frame_ms_budget.map(round),
                "slow": frame_ms_budget.is_some_and(|b| self.frame_ms > b),
                "cpuLayers": self.cpu.len(),
                "remote": self.remote,
            },
            "layers": layers,
        })
    }

    fn ms(&self, key: (ItemId, LayerId)) -> f64 {
        self.layers.get(&key).copied().unwrap_or(0.0)
    }
}

/// `why` in the interface language (the Render Time pane's hover, the toast).
pub fn why_text(why: CpuWhy) -> String {
    crate::i18n::tr_args(why.why, &[&why.what])
}

/// A slow frame with CPU work in it, said once in a toast (RI1): `layer` (`None`: the whole
/// frame) of `comp` rendered on the CPU because of `why`, and the frame took `ms`.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuNotice {
    pub comp: ItemId,
    pub comp_name: String,
    pub layer: Option<LayerId>,
    pub layer_name: Option<String>,
    pub why: CpuWhy,
    pub ms: f64,
}

/// Notices waiting for the UI, at most.
const NOTICES_KEPT: usize = 8;

/// The notices said this session (by comp, layer and reason) and those waiting to be shown.
#[derive(Default)]
struct Notices {
    said: std::collections::HashSet<(ItemId, Option<LayerId>, CpuWhy)>,
    waiting: VecDeque<CpuNotice>,
}

impl CpuNotice {
    /// The toast: "Shape Layer 1 rendered on the CPU (Taper or Wave stroke: …) · 48 ms".
    pub fn message(&self) -> String {
        let name = self.layer_name.as_deref().unwrap_or(&self.comp_name);
        crate::i18n::tr_args("{} rendered on the CPU ({}) · {} ms", &[&name, &why_text(self.why), &format!("{:.0}", self.ms)])
    }
}

impl Notices {
    /// `ft` (a frame of `comp` in `project`) as a notice, when it was slower than one frame of
    /// the comp and its CPU work wasn't said before: the whole frame's reason, else the slowest
    /// CPU layer's not said yet.
    fn notice(&mut self, project: &Project, comp: ItemId, ft: &FrameTimes) -> Option<CpuNotice> {
        let c = project.comp(comp)?;
        let fps = c.frame_rate.as_f64();
        if !(fps.is_finite() && fps > 0.0) || !ft.frame_ms.is_finite() || ft.frame_ms <= 1000.0 / fps {
            return None;
        }
        let (layer, why) = match ft.frame_cpu {
            Some(why) => (None, why),
            None => {
                let ms = |k: &(ItemId, LayerId)| ft.layers.get(k).copied().unwrap_or(0.0);
                let ((lc, l), why) = ft
                    .cpu
                    .iter()
                    .filter(|((lc, l), why)| !self.said.contains(&(*lc, Some(*l), **why)))
                    .max_by(|a, b| ms(a.0).total_cmp(&ms(b.0)))
                    .map(|(k, w)| (*k, *w))?;
                (Some((lc, l)), why)
            }
        };
        let key = (layer.map_or(comp, |(lc, _)| lc), layer.map(|(_, l)| l), why);
        if !self.said.insert(key) {
            return None;
        }
        let layer_name = layer.and_then(|(lc, l)| project.comp(lc)?.layer(l)).map(|l| l.name.clone());
        let comp_name = project.item(key.0).map(|i| i.name.clone()).unwrap_or_default();
        Some(CpuNotice { comp: key.0, comp_name, layer: key.1, layer_name, why, ms: ft.frame_ms })
    }
}

/// Layer times kept at most (about 40 bytes each): the oldest frames' times go first.
const BUDGET: usize = 1 << 18;

/// The times of the frames rendered lately, by frame (its content, not its revision), and the
/// slow CPU fallbacks to say in a toast (RI1).
#[derive(Default)]
pub struct RenderTimes {
    kept: Mutex<Kept>,
    notices: Mutex<Notices>,
}

#[derive(Default)]
struct Kept {
    map: HashMap<FrameId, Arc<FrameTimes>>,
    /// Oldest first.
    order: VecDeque<FrameId>,
    layers: usize,
}

type FrameId = (u64, u64, i64, u32, u64, u64);

impl RenderTimes {
    /// Keep `times` as frame `key`'s (replacing older ones).
    pub fn put(&self, key: &FrameKey, times: FrameTimes) {
        let id = key.id();
        let mut k = self.kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = k.map.remove(&id) {
            k.layers = k.layers.saturating_sub(old.layers.len());
            k.order.retain(|o| *o != id);
        }
        k.layers = k.layers.saturating_add(times.layers.len());
        k.map.insert(id, Arc::new(times));
        k.order.push_back(id);
        while k.layers > BUDGET && k.order.len() > 1 {
            let Some(oldest) = k.order.pop_front() else { break };
            if let Some(old) = k.map.remove(&oldest) {
                k.layers = k.layers.saturating_sub(old.layers.len());
            }
        }
    }

    /// Frame `key`'s times, if it rendered lately.
    pub fn get(&self, key: &FrameKey) -> Option<Arc<FrameTimes>> {
        self.kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner).map.get(&key.id()).cloned()
    }

    /// Forget every frame's times (what the toasts said stays said).
    pub fn clear(&self) {
        *self.kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Kept::default();
    }

    /// A viewer frame `ft` of `comp` ran on the CPU (in part): queue a toast if it was slow and
    /// this wasn't said before this session ([`Notices::notice`]).
    pub fn notice_slow_cpu(&self, project: &Project, comp: ItemId, ft: &FrameTimes) {
        let mut n = self.notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(notice) = n.notice(project, comp, ft) {
            if n.waiting.len() >= NOTICES_KEPT {
                n.waiting.pop_front();
            }
            n.waiting.push_back(notice);
        }
    }

    /// The oldest notice waiting for a toast.
    pub fn take_cpu_notice(&self) -> Option<CpuNotice> {
        self.notices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).waiting.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(frame: i64) -> FrameKey {
        FrameKey { revision: 1, content: 1, comp: 1, frame, scale: 1000, view: 0, opts: 0 }
    }

    fn times(n: u64) -> FrameTimes {
        FrameTimes { frame_ms: 1.0, layers: (0..n).map(|i| ((ItemId(1), LayerId(i)), 1.0)).collect(), ..FrameTimes::default() }
    }

    #[test]
    fn a_frame_keeps_its_latest_times_and_old_frames_go_past_the_budget() {
        let t = RenderTimes::default();
        t.put(&key(0), times(2));
        t.put(&FrameKey { revision: 2, ..key(0) }, times(3));
        // Another revision with the same content is the same frame.
        assert_eq!(t.get(&key(0)).map(|f| f.layers.len()), Some(3));
        let per = 1 << 16;
        for f in 1..=4 {
            t.put(&key(f), times(per));
        }
        assert!(t.get(&key(0)).is_none(), "the oldest frame's times go first");
        assert!(t.get(&key(4)).is_some());
        assert_eq!(t.get(&key(4)).and_then(|f| f.layer(ItemId(1), LayerId(7))), Some(1.0));
    }

    /// A 25 fps comp (40 ms a frame) with two shape layers.
    fn comp() -> (Project, ItemId, LayerId, LayerId) {
        use effectcraft_engine::project::{Comp, ItemKind, LayerSource, build};
        let mut p = Project::default();
        let mut c = Comp::new(320, 180, effectcraft_time::FrameRate::FPS_25, effectcraft_time::Tick::from_seconds_f64(2.0));
        let a = build::layer(&mut p, &c, "Shape Layer 1", LayerSource::Shape, (320, 180), None);
        let b = build::layer(&mut p, &c, "Shape Layer 2", LayerSource::Shape, (320, 180), None);
        let (la, lb) = (a.id, b.id);
        c.layers.push(a);
        c.layers.push(b);
        let id = p.add_item("Card", Default::default(), None, ItemKind::Comp(c.into()));
        (p, id, la, lb)
    }

    fn cpu_frame(comp: ItemId, ms: f64, cpu: &[(LayerId, f64, CpuWhy)]) -> FrameTimes {
        FrameTimes {
            frame_ms: ms,
            layers: cpu.iter().map(|(l, ms, _)| ((comp, *l), *ms)).collect(),
            cpu: cpu.iter().map(|(l, _, w)| ((comp, *l), *w)).collect(),
            ..FrameTimes::default()
        }
    }

    #[test]
    fn a_slow_cpu_layer_is_said_once_per_layer_and_reason() {
        use effectcraft_engine::render::cpu_why;
        let (p, comp, la, lb) = comp();
        let t = RenderTimes::default();
        // Within a frame's time (40 ms at 25 fps): nothing to say.
        t.notice_slow_cpu(&p, comp, &cpu_frame(comp, 30.0, &[(la, 20.0, cpu_why::TAPER_STROKE)]));
        assert_eq!(t.take_cpu_notice(), None);
        // Slow: the slowest CPU layer, by name.
        t.notice_slow_cpu(&p, comp, &cpu_frame(comp, 48.0, &[(la, 30.0, cpu_why::TAPER_STROKE), (lb, 5.0, cpu_why::LAYER_STYLES)]));
        let n = t.take_cpu_notice().unwrap();
        assert_eq!((n.layer, n.layer_name.as_deref(), n.why, n.ms), (Some(la), Some("Shape Layer 1"), cpu_why::TAPER_STROKE, 48.0));
        assert_eq!(t.take_cpu_notice(), None);
        // The same again: not repeated; the other layer's reason is new, so it's said.
        t.notice_slow_cpu(&p, comp, &cpu_frame(comp, 50.0, &[(la, 30.0, cpu_why::TAPER_STROKE), (lb, 5.0, cpu_why::LAYER_STYLES)]));
        assert_eq!(t.take_cpu_notice().map(|n| (n.layer, n.why)), Some((Some(lb), cpu_why::LAYER_STYLES)));
        t.notice_slow_cpu(&p, comp, &cpu_frame(comp, 50.0, &[(la, 30.0, cpu_why::TAPER_STROKE), (lb, 5.0, cpu_why::LAYER_STYLES)]));
        assert_eq!(t.take_cpu_notice(), None);
        // A changed reason is said again.
        t.notice_slow_cpu(&p, comp, &cpu_frame(comp, 50.0, &[(la, 30.0, cpu_why::PATH_TOO_BIG)]));
        assert_eq!(t.take_cpu_notice().map(|n| n.why), Some(cpu_why::PATH_TOO_BIG));
        // Purging the frames doesn't make it say them again.
        t.clear();
        t.notice_slow_cpu(&p, comp, &cpu_frame(comp, 50.0, &[(la, 30.0, cpu_why::PATH_TOO_BIG)]));
        assert_eq!(t.take_cpu_notice(), None);
    }

    #[test]
    fn a_whole_cpu_frame_is_said_once_per_comp_and_tags_every_layer() {
        use effectcraft_engine::render::cpu_why;
        let (p, comp, la, _) = comp();
        let t = RenderTimes::default();
        let mut f = cpu_frame(comp, 90.0, &[]);
        f.layers.insert((comp, la), 80.0);
        f.frame_cpu = Some(cpu_why::GPU_OUT_OF_MEMORY);
        assert_eq!(f.layer_cpu(comp, la), Some(cpu_why::GPU_OUT_OF_MEMORY));
        t.notice_slow_cpu(&p, comp, &f);
        let n = t.take_cpu_notice().unwrap();
        assert_eq!((n.layer, n.comp_name.as_str(), n.why), (None, "Card", cpu_why::GPU_OUT_OF_MEMORY));
        t.notice_slow_cpu(&p, comp, &f);
        assert_eq!(t.take_cpu_notice(), None);
        // A GPU frame tags only its CPU layers; a precomp shows its nested comp's slowest one.
        let g = cpu_frame(comp, 10.0, &[(la, 4.0, cpu_why::MASK_FEATHER)]);
        assert_eq!(g.layer_cpu(comp, LayerId(999)), None);
        assert_eq!(g.nested_cpu(comp), Some(cpu_why::MASK_FEATHER));
        assert_eq!(g.nested_cpu(ItemId(12345)), None);
    }

    #[test]
    fn render_times_json_says_where_each_layer_ran_and_why() {
        use effectcraft_engine::render::{CpuWhy, cpu_why};
        let (p, comp, la, lb) = comp();
        let mut f = cpu_frame(comp, 48.0, &[(la, 30.0, CpuWhy::named(cpu_why::EFFECT_NO_GPU.why, "Turbulent Displace"))]);
        f.layers.insert((comp, lb), 2.0);
        let v = f.to_json(&p, comp);
        assert_eq!(v["frame"]["backend"], "gpu");
        assert_eq!(v["frame"]["slow"], true);
        assert_eq!(v["frame"]["frameDurationMs"], 40.0);
        assert_eq!(v["layers"][0]["name"], "Shape Layer 1");
        assert_eq!(v["layers"][0]["backend"], "cpu");
        assert_eq!(v["layers"][0]["reason"], "effect Turbulent Displace has no GPU version");
        assert_eq!(v["layers"][1]["backend"], "gpu");
        assert!(v["layers"][1]["reason"].is_null());
        f.frame_cpu = Some(cpu_why::GPU_OFF);
        let v = f.to_json(&p, comp);
        assert_eq!((v["frame"]["backend"].as_str(), v["frame"]["reason"].as_str()), (Some("cpu"), Some("GPU acceleration is off")));
        assert_eq!(v["layers"][1]["backend"], "cpu", "every layer of a CPU frame ran on the CPU");
    }
}
