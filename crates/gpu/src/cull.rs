//! Compositing work the GPU walk skips because it cannot change the frame (the predicates live
//! in `effectcraft_render::cull`, next to the CPU compositor whose semantics they encode):
//!
//! - layers off the canvas (the CPU compositor's own test, so both cull the same layers) and
//!   layers behind an empty (non-inverted) track matte draw nothing: no source, effects or
//!   upload;
//! - layers under an opaque solid that covers the whole canvas are overwritten pixel for pixel
//!   (Normal over with source alpha 1 returns the source): not drawn;
//! - identical precomp instances (the same nested comp at the same nested time through equal
//!   renderers) render their nested frame once per frame, in the same comp or in different
//!   comps of the frame (an icon precomp in several screens); each instance then runs its own
//!   effects, masks, transform and blend on it.

use effectcraft_project::{ItemId, Layer, LayerSource};
use effectcraft_render::cull::NestedKey;
use effectcraft_render::{EvalCtx, Renderer};
use effectcraft_time::Tick;

use crate::context::Enc;

/// Nested comp frames kept for the other instances of their precomp within one frame.
#[derive(Default)]
pub(crate) struct Shared {
    entries: Vec<Entry>,
    texels: u64,
    /// Precomp layers showing each comp, in the whole project (counted once per frame).
    uses: std::collections::HashMap<ItemId, usize>,
}

struct Entry {
    key: NestedKey,
    img: crate::codes::Pixels,
    /// Instances that may still ask for it (the project's other layers with the same source;
    /// what's left is dropped with the frame).
    uses: usize,
}

/// Bounds on what a frame keeps for sharing (beyond them, instances render again).
const MAX_ENTRIES: usize = 64;
const MAX_TEXELS: u64 = 1 << 25;

#[cfg(test)]
thread_local! {
    /// Tests: render without any of these skips (the reference walk).
    pub(crate) static OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Tests: what was skipped on this thread.
    pub(crate) static COUNTS: std::cell::Cell<Counts> = const { std::cell::Cell::new(Counts { off_canvas: 0, matte: 0, occluded: 0, nested_renders: 0, nested_shared: 0 }) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Counts {
    pub(crate) off_canvas: usize,
    pub(crate) matte: usize,
    pub(crate) occluded: usize,
    pub(crate) nested_renders: usize,
    pub(crate) nested_shared: usize,
}

#[cfg(test)]
fn count(f: impl FnOnce(&mut Counts)) {
    COUNTS.with(|c| {
        let mut v = c.get();
        f(&mut v);
        c.set(v);
    });
}

#[cfg(not(test))]
fn enabled() -> bool {
    true
}

#[cfg(test)]
fn enabled() -> bool {
    !OFF.with(std::cell::Cell::get)
}

/// A 2D layer `walk::draw_layer` leaves out (after its wireframe, adjustment and opacity
/// tests, where the CPU compositor makes the same off-canvas test): it draws nothing on a
/// `size` canvas.
pub(crate) fn skip_layer(r: &Renderer, ctx: &EvalCtx, layer: &Layer, size: (u32, u32)) -> bool {
    if !enabled() {
        return false;
    }
    if r.culled_off_canvas(ctx, layer, size) {
        #[cfg(test)]
        count(|c| c.off_canvas += 1);
        return true;
    }
    skip_matte(r, ctx, layer)
}

/// The layer's track matte is certainly empty: the matted layer adds nothing.
pub(crate) fn skip_matte(r: &Renderer, ctx: &EvalCtx, layer: &Layer) -> bool {
    let skip = enabled() && r.matte_empty(ctx, layer);
    #[cfg(test)]
    if skip {
        count(|c| c.matte += 1);
    }
    skip
}

/// The index in `visible` (bottom to top) `walk::draw_layers` starts at: the topmost layer that
/// replaces every pixel of the `size` canvas; the layers below it are overwritten.
pub(crate) fn first_drawn(r: &Renderer, ctx: &EvalCtx, visible: &[&Layer], size: (u32, u32)) -> usize {
    if !enabled() {
        return 0;
    }
    let k = visible.iter().rposition(|l| r.opaque_cover(ctx, l, size)).unwrap_or(0);
    #[cfg(test)]
    count(|c| c.occluded += k);
    k
}

/// `walk::render` of a precomp layer's nested comp (`sub` drawing `item` at `t`), shared with
/// the frame's other instances of `item` (in this comp or another) that render it identically.
pub(crate) fn nested(e: &mut Enc, ctx: &EvalCtx, sub: &Renderer, item: ItemId, t: Tick) -> Option<crate::codes::Pixels> {
    let others = instances(&mut e.shared, ctx, item).saturating_sub(1);
    let key = if enabled() && others > 0 { sub.nested_key(item, t) } else { None };
    let Some(key) = key else {
        #[cfg(test)]
        count(|c| c.nested_renders += 1);
        return crate::walk::render(e, sub, item, t);
    };
    let shared = &mut e.shared;
    if let Some(i) = shared.entries.iter().position(|x| x.key == key)
        && let Some(entry) = shared.entries.get_mut(i)
    {
        let img = entry.img.clone();
        entry.uses = entry.uses.saturating_sub(1);
        if entry.uses == 0 {
            let gone = shared.entries.swap_remove(i);
            shared.texels = shared.texels.saturating_sub(texels(&gone.img));
        }
        #[cfg(test)]
        count(|c| c.nested_shared += 1);
        return Some(img);
    }
    #[cfg(test)]
    count(|c| c.nested_renders += 1);
    let img = crate::walk::render(e, sub, item, t)?;
    let shared = &mut e.shared;
    let n = texels(&img);
    if shared.entries.len() < MAX_ENTRIES && shared.texels.saturating_add(n) <= MAX_TEXELS {
        shared.texels = shared.texels.saturating_add(n);
        shared.entries.push(Entry { key, img: img.clone(), uses: others });
    }
    Some(img)
}

/// Precomp layers of the project showing `item` (at least the comp's own).
fn instances(shared: &mut Shared, ctx: &EvalCtx, item: ItemId) -> usize {
    let here = ctx.comp.layers.iter().filter(|l| matches!(l.source, LayerSource::Comp { item: i } if i == item)).count();
    if here == 0 {
        return 0;
    }
    let all = *shared.uses.entry(item).or_insert_with(|| {
        ctx.project
            .items
            .values()
            .filter_map(|it| match &it.kind {
                effectcraft_project::ItemKind::Comp(c) => {
                    Some(c.layers.iter().filter(|l| matches!(l.source, LayerSource::Comp { item: i } if i == item)).count())
                }
                _ => None,
            })
            .sum()
    });
    all.max(here)
}

fn texels(img: &crate::codes::Pixels) -> u64 {
    u64::from(img.width()).saturating_mul(u64::from(img.height()))
}
