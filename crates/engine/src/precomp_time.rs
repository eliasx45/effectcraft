//! Time through precomps: a chain of precomp layers ([`Hop`]s, outermost first) maps a time in
//! an inner comp to the comp around it through each layer's start and stretch. `keys.alignPeak`
//! uses it to land nested keys on an outer comp's frame; the `off-accent` lint and
//! `speech.accents` use it to see nested speed peaks. Time-remapped precomp layers can't be mapped.

use effectcraft_project::{ItemId, Layer, LayerId, LayerSource, Project};

/// Deepest precomp nesting followed.
pub const MAX_DEPTH: usize = 12;

/// One precomp layer between an outer comp and an inner one: its start and stretch map the outer
/// comp's time to the precomp's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hop {
    /// The comp the precomp layer is in.
    pub comp: ItemId,
    pub layer: LayerId,
    /// Outer comp seconds.
    pub start: f64,
    /// Percent.
    pub stretch: f64,
}

/// Outer comp seconds → the innermost comp's seconds.
pub fn down(chain: &[Hop], t: f64) -> f64 {
    chain.iter().fold(t, |t, h| (t - h.start) * 100.0 / h.stretch)
}

/// The innermost comp's seconds → outer comp seconds.
pub fn up(chain: &[Hop], t: f64) -> f64 {
    chain.iter().rev().fold(t, |t, h| h.start + t * h.stretch / 100.0)
}

/// Seconds of the inner comp per second of the outer comp.
pub fn scale(chain: &[Hop]) -> f64 {
    chain.iter().fold(1.0, |k, h| k * 100.0 / h.stretch.abs())
}

/// `l` (a layer of `comp`) as a hop, or why its time can't be mapped.
pub fn hop(comp: ItemId, l: &Layer) -> std::result::Result<Hop, String> {
    if l.props.get("timeRemap").is_some_and(|t| !t.keys.is_empty() || t.has_expression()) {
        return Err(format!("{} is time-remapped, so its time can't be mapped to the comp around it", l.name));
    }
    if !l.stretch.is_finite() || l.stretch.abs() < 1e-6 {
        return Err(format!("{} has a zero time stretch", l.name));
    }
    Ok(Hop { comp, layer: l.id, start: l.start_time.seconds(), stretch: l.stretch })
}

/// Every chain of precomp layers from `outer` down to `inner` (at most 8).
pub fn chains(project: &Project, outer: ItemId, inner: ItemId) -> Vec<Vec<(ItemId, LayerId)>> {
    fn walk(project: &Project, at: ItemId, inner: ItemId, path: &mut Vec<(ItemId, LayerId)>, out: &mut Vec<Vec<(ItemId, LayerId)>>) {
        if out.len() >= 8 || path.len() >= MAX_DEPTH {
            return;
        }
        let Some(comp) = project.comp(at) else { return };
        for l in &comp.layers {
            let LayerSource::Comp { item } = l.source else { continue };
            if path.iter().any(|(c, _)| *c == item) || item == at {
                continue;
            }
            path.push((at, l.id));
            if item == inner {
                out.push(path.clone());
            } else {
                walk(project, item, inner, path, out);
            }
            path.pop();
        }
    }
    let mut out = vec![];
    if outer != inner {
        walk(project, outer, inner, &mut vec![], &mut out);
    }
    out
}
