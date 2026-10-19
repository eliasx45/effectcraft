//! Which layers are too heavy to play in real time, from the render times the viewer measures
//! (the Render Time pane's per-layer milliseconds). The viewer reports each frame it rendered
//! ([`Session::note_layer_costs`]); `freeze.suggest` lists the layers whose time stays far above
//! the frame budget, and Smart Freeze can cache them. Nothing is frozen from here unless the
//! `previews.freezeAutoHeavy` preference is on.

use std::collections::{BTreeMap, BTreeSet};

use effectcraft_project::freeze::Target;
use effectcraft_project::{ItemId, LayerId, LayerSource};
use serde_json::{Value, json};

use crate::Session;

/// A layer is heavy when its frames take more than this many frame budgets.
pub const HEAVY_FACTOR: f64 = 2.0;
/// Frames measured before a layer can be suggested ("for a whole section", not one slow frame).
pub const MIN_SAMPLES: u32 = 8;
/// How many of the latest frames count.
const WINDOW: u32 = 16;
/// At least this share of the latest frames must be heavy.
const HEAVY_SHARE: f64 = 0.8;
/// Layers tracked at most.
const MAX_ENTRIES: usize = 4096;
/// Suggestions listed at most.
const MAX_SUGGESTIONS: usize = 8;
/// Seconds without editing or playing before `previews.freezeAutoHeavy` freezes a section.
pub const AUTO_IDLE_SECONDS: u64 = 8;

#[derive(Clone, Copy, Debug)]
struct Cost {
    /// Moving average of the layer's time (ms).
    ema_ms: f64,
    /// The frame budget the average is compared with (ms).
    budget_ms: f64,
    /// One bit per measured frame, newest lowest: set when the frame was heavy.
    heavy: u32,
    samples: u32,
}

impl Cost {
    fn window(&self) -> u32 {
        self.samples.min(WINDOW)
    }

    fn mask(&self) -> u32 {
        (1u32 << self.window()) - 1
    }

    /// Whether this layer stays far above the frame budget.
    fn is_heavy(&self) -> bool {
        let n = self.window();
        self.samples >= MIN_SAMPLES
            && f64::from((self.heavy & self.mask()).count_ones()) >= HEAVY_SHARE * f64::from(n)
            && self.ema_ms > HEAVY_FACTOR * self.budget_ms
    }
}

/// The layers' recent render times and the suggestions the user turned down.
#[derive(Default)]
pub struct CostBook {
    layers: BTreeMap<(ItemId, LayerId), Cost>,
    dismissed: BTreeSet<(ItemId, LayerId)>,
    /// Layers `previews.freezeAutoHeavy` already tried (a failed or finished attempt is not
    /// repeated until the layer's cost is measured again).
    tried: BTreeSet<(ItemId, LayerId)>,
}

impl CostBook {
    /// Forget everything measured (a new project).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn note(&mut self, comp: ItemId, layer: LayerId, ms: f64, budget_ms: f64) {
        if !ms.is_finite() || ms < 0.0 || !budget_ms.is_finite() || budget_ms <= 0.0 {
            return;
        }
        let key = (comp, layer);
        if !self.layers.contains_key(&key) && self.layers.len() >= MAX_ENTRIES {
            self.layers.pop_first();
        }
        let heavy = ms > HEAVY_FACTOR * budget_ms;
        let cost = self.layers.entry(key).or_insert(Cost { ema_ms: ms, budget_ms, heavy: 0, samples: 0 });
        cost.ema_ms = if cost.samples == 0 { ms } else { 0.7 * cost.ema_ms + 0.3 * ms };
        cost.budget_ms = budget_ms;
        cost.heavy = (cost.heavy << 1 | u32::from(heavy)) & ((1 << WINDOW) - 1);
        cost.samples = cost.samples.saturating_add(1);
        if !cost.is_heavy() {
            self.tried.remove(&key);
        }
    }
}

/// What freezing a suggestion freezes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// The precomposition the heavy layer shows.
    Precomp(ItemId),
    /// The heavy layer alone.
    Layer,
    /// The whole composition the layer is in: `freeze.freeze` refuses the layer alone (for
    /// example the comp has expressions), so its comp is the unit that can be frozen.
    Comp,
}

/// One heavy layer, ready for `freeze.freeze`.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub comp: ItemId,
    pub layer: LayerId,
    pub name: String,
    /// The name of `comp`.
    pub comp_name: String,
    pub ms_per_frame: f64,
    pub budget_ms: f64,
    pub samples: u32,
    pub unit: Unit,
}

impl Suggestion {
    /// How many frame budgets a frame takes.
    pub fn ratio(&self) -> f64 {
        self.ms_per_frame / self.budget_ms
    }

    /// The parameters `freeze.freeze` takes for this suggestion.
    pub fn freeze_params(&self) -> Value {
        match self.unit {
            Unit::Comp => json!({"comp": self.comp.0, "mode": "comp", "auto": true}),
            Unit::Precomp(_) => json!({"comp": self.comp.0, "layers": [self.layer.0], "mode": "precomp", "auto": true}),
            Unit::Layer => json!({"comp": self.comp.0, "layers": [self.layer.0], "mode": "selection", "auto": true}),
        }
    }

    /// "Stat Stack renders at 1.8 s/frame"
    pub fn hint(&self) -> String {
        let time =
            if self.ms_per_frame >= 1000.0 { format!("{:.1} s/frame", self.ms_per_frame / 1000.0) } else { format!("{:.0} ms/frame", self.ms_per_frame) };
        format!("{} renders at {time}", self.name)
    }

    /// The Timeline's question: "Stat Stack renders at 1.8 s/frame — Freeze it?", or, when only
    /// the whole comp can be frozen, "Freeze Comp 28 (C28 Counter renders at 144 ms/frame)".
    pub fn question(&self) -> String {
        match self.unit {
            Unit::Comp => format!("Freeze {} ({})", self.comp_name, self.hint()),
            _ => format!("{} — Freeze it?", self.hint()),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "comp": self.comp.0,
            "layer": self.layer.0,
            "name": self.name,
            "compName": self.comp_name,
            "msPerFrame": self.ms_per_frame,
            "frameBudgetMs": self.budget_ms,
            "ratio": self.ratio(),
            "samples": self.samples,
            "target": match self.unit { Unit::Comp => "comp", Unit::Precomp(_) => "precomp", Unit::Layer => "selection" },
            "question": self.question(),
            "freeze": self.freeze_params(),
        })
    }
}

impl Session {
    /// The viewer rendered a frame of `comp`: each layer's time in ms, as the Render Time pane
    /// shows it (`(comp the layer is in, layer, ms)`; precomp layers are inclusive).
    pub fn note_layer_costs(&mut self, comp: ItemId, costs: impl IntoIterator<Item = (ItemId, LayerId, f64)>) {
        let Some(rate) = self.project.comp(comp).map(|c| c.frame_rate) else {
            return;
        };
        if rate.num == 0 || rate.den == 0 {
            return;
        }
        let budget_ms = 1000.0 / rate.as_f64();
        for (in_comp, layer, ms) in costs {
            self.freeze_runtime.cost.note(in_comp, layer, ms, budget_ms);
        }
    }

    /// Heavy layers of the active composition that `freeze.freeze` can freeze (see
    /// [`Self::freeze_suggestions_in`]).
    pub fn freeze_suggestions(&self) -> Vec<Suggestion> {
        self.freeze_suggestions_in(self.active_comp_id())
    }

    /// Layers of `scope` (every composition when `None`) that stay far above the frame budget
    /// and are not frozen or dismissed, the slowest first. Only what `freeze.freeze` accepts is
    /// suggested: a layer it refuses on its own (for example one with expressions) is
    /// offered as its whole composition when that can be frozen, else not at all. A layer inside
    /// a precomp that is itself suggested is left out: freezing the precomp covers it.
    pub fn freeze_suggestions_in(&self, scope: Option<ItemId>) -> Vec<Suggestion> {
        let book = &self.freeze_runtime.cost;
        let mut found: Vec<Suggestion> = Vec::new();
        for (&(comp_id, layer_id), cost) in &book.layers {
            if !cost.is_heavy() || book.dismissed.contains(&(comp_id, layer_id)) || scope.is_some_and(|c| c != comp_id) {
                continue;
            }
            let Some(comp) = self.project.comp(comp_id) else {
                continue;
            };
            let Some(layer) = comp.layer(layer_id) else {
                continue;
            };
            if !layer.switches.video {
                continue;
            }
            let precomp = match layer.source {
                LayerSource::Comp { item } => Some(item),
                _ => None,
            };
            let frozen = self.project.freeze.sections.iter().any(|s| {
                s.enabled
                    && match &s.target {
                        Target::Comp { comp } => *comp == comp_id,
                        Target::Precomp { comp } => Some(*comp) == precomp || *comp == comp_id,
                        Target::Selection { comp, layers } => *comp == comp_id && layers.contains(&layer_id),
                    }
            });
            if frozen {
                continue;
            }
            let own = match precomp {
                Some(item) => (Target::Precomp { comp: item }, Unit::Precomp(item)),
                None => (Target::Selection { comp: comp_id, layers: vec![layer_id] }, Unit::Layer),
            };
            let unit = if crate::freeze::validate_target(&self.project, &own.0).is_ok() {
                own.1
            } else if crate::freeze::validate_target(&self.project, &Target::Comp { comp: comp_id }).is_ok() {
                Unit::Comp
            } else {
                continue;
            };
            let comp_name = self.project.item(comp_id).map(|i| i.name.clone()).unwrap_or_default();
            let candidate = Suggestion {
                comp: comp_id,
                layer: layer_id,
                name: layer.name.clone(),
                comp_name,
                ms_per_frame: cost.ema_ms,
                budget_ms: cost.budget_ms,
                samples: cost.samples,
                unit,
            };
            // One whole-comp suggestion per composition: its slowest layer.
            match found.iter_mut().find(|f| f.unit == Unit::Comp && unit == Unit::Comp && f.comp == comp_id) {
                Some(existing) if existing.ms_per_frame >= candidate.ms_per_frame => {}
                Some(existing) => *existing = candidate,
                None => found.push(candidate),
            }
        }
        let outer: BTreeSet<ItemId> = found.iter().filter_map(|f| if let Unit::Precomp(item) = f.unit { Some(item) } else { None }).collect();
        found.retain(|f| !outer.contains(&f.comp));
        found.sort_by(|a, b| b.ms_per_frame.total_cmp(&a.ms_per_frame));
        found.truncate(MAX_SUGGESTIONS);
        found
    }

    /// Stop suggesting `layer` of `comp` this session.
    pub fn dismiss_freeze_suggestion(&mut self, comp: ItemId, layer: LayerId) {
        self.freeze_runtime.cost.dismissed.insert((comp, layer));
    }

    /// `previews.freezeAutoHeavy`: freeze the slowest heavy layer once the user has been idle.
    /// Called from [`Session::poll_freeze`]; one section at a time, and each layer once.
    pub(crate) fn auto_freeze_heavy(&mut self) {
        if cfg!(target_arch = "wasm32") || !self.prefs.previews.freeze_auto_heavy {
            return;
        }
        if self.freeze_runtime.activity.elapsed().as_secs() < AUTO_IDLE_SECONDS || self.freeze_runtime.changed.elapsed().as_secs() < AUTO_IDLE_SECONDS {
            return;
        }
        if self.tasks.iter().any(|t| matches!(t.kind, "smartFreeze" | "freezeCheck" | "freezeStorage")) {
            return;
        }
        let Some(pick) = self
            .freeze_suggestions_in(None)
            .into_iter()
            .find(|s| !self.freeze_runtime.cost.tried.contains(&(s.comp, s.layer)) && self.claims.get(s.comp).is_none())
        else {
            return;
        };
        self.freeze_runtime.cost.tried.insert((pick.comp, pick.layer));
        if let Err(e) = self.execute("freeze.freeze", pick.freeze_params()) {
            self.toast(format!("Auto Freeze of {}: {e}", pick.name));
        }
    }
}
