//! Why a layer or a frame rendered on the CPU instead of the GPU (RI1), in plain words for the
//! Timeline's Render Time pane, its toast and the control channel (`ui.renderTimes`).
//!
//! A reason is a [`CpuWhy`]: a fixed sentence, with `{}` standing for an optional name (an
//! effect's), so recording one never allocates. The GPU compositor records a layer's reason in
//! [`crate::LayerTimes`] where it hands the layer to the CPU ([`crate::Renderer::note_cpu`]);
//! the planners deep inside (shape, text and mask plans) can't see the layer's times, so they
//! leave their reason in a per-thread slot ([`declined`]) that the caller takes ([`take`]) when
//! the plan comes back empty.

use std::cell::Cell;

/// One reason something rendered on the CPU: `why` with `{}` replaced by `what` (when not
/// empty). The UI translates `why` (`tr_args`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CpuWhy {
    pub why: &'static str,
    pub what: &'static str,
}

impl CpuWhy {
    /// A reason without a name.
    pub const fn new(why: &'static str) -> CpuWhy {
        CpuWhy { why, what: "" }
    }

    /// A reason naming `what` (an effect).
    pub const fn named(why: &'static str, what: &'static str) -> CpuWhy {
        CpuWhy { why, what }
    }

    /// The reason in English (the control channel, logs).
    pub fn text(&self) -> String {
        if self.why.contains("{}") { self.why.replacen("{}", self.what, 1) } else { self.why.to_owned() }
    }
}

impl std::fmt::Display for CpuWhy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

// Whole frames.
pub const GPU_OFF: CpuWhy = CpuWhy::new("GPU acceleration is off");
pub const NO_GPU: CpuWhy = CpuWhy::new("no GPU is available");
pub const GPU_RETIRED: CpuWhy = CpuWhy::new("the GPU stopped working this session");
pub const GPU_OUT_OF_MEMORY: CpuWhy = CpuWhy::new("the GPU ran out of memory");
pub const GPU_DECLINED_FRAME: CpuWhy = CpuWhy::new("the GPU declined this frame");
pub const GPU_DECLINED_COMP: CpuWhy = CpuWhy::new("the GPU declined this comp earlier");
pub const GPU_UNFINISHED: CpuWhy = CpuWhy::new("the GPU didn't finish the frame");
pub const FROZEN_PREVIEW: CpuWhy = CpuWhy::new("a frozen preview section");
pub const RETRY: CpuWhy = CpuWhy::new("rendered again after a failed GPU attempt");
pub const CPU_FASTER: CpuWhy = CpuWhy::new("Auto measured the CPU as faster for this comp");

// Layers.
pub const TAPER_STROKE: CpuWhy = CpuWhy::new("Taper or Wave stroke: the GPU can't draw it");
pub const SHAPE_BLEND: CpuWhy = CpuWhy::new("a shape drawn in a blend mode other than Normal");
pub const PATH_TOO_BIG: CpuWhy = CpuWhy::new("a path or mask wider than 8191 px");
pub const TOO_MANY_EDGES: CpuWhy = CpuWhy::new("too many path edges for the GPU");
pub const GPU_BUFFER_LIMITS: CpuWhy = CpuWhy::new("too large for the GPU's buffers");
pub const SHAPE_MOTION_BLUR: CpuWhy = CpuWhy::new("motion-blurred shape content");
pub const TEXT_ANIMATORS: CpuWhy = CpuWhy::new("text Blur, Gradient Fill or Inter-Character Blending");
pub const TEXT_BLUR: CpuWhy = CpuWhy::new("blurred or gradient-filled characters");
pub const MASK_FEATHER: CpuWhy = CpuWhy::new("a mask with variable-width feather");
pub const MASK_MOTION_BLUR: CpuWhy = CpuWhy::new("mask motion blur on an animated path");
pub const MASK_EXPANSION: CpuWhy = CpuWhy::new("a negative or very large mask expansion");
pub const GPU_SHAPES_OFF: CpuWhy = CpuWhy::new("GPU shape and text drawing is turned off (EFFECTCRAFT_GPU_SHAPES / _TEXT)");
pub const RASTER_DECLINED: CpuWhy = CpuWhy::new("the GPU can't draw this layer's content");
pub const LAYER_STYLES: CpuWhy = CpuWhy::new("layer styles the GPU doesn't render");
pub const STYLE_TOO_WIDE: CpuWhy = CpuWhy::new("layer style {} reaches farther than the GPU's distance field");
pub const STYLE_STENCIL: CpuWhy = CpuWhy::new("a Drop Shadow or Outer Glow in a Stencil or Silhouette mode");
pub const STYLES_REVISITED: CpuWhy = CpuWhy::new("layer styles drawn once on the CPU to cache a revisited frame");
pub const EFFECT_NO_GPU: CpuWhy = CpuWhy::new("effect {} has no GPU version");
pub const EFFECT_DECLINED: CpuWhy = CpuWhy::new("the GPU declined effect {}");
pub const ADJUSTMENT: CpuWhy = CpuWhy::new("an adjustment layer the GPU can't apply");
pub const COLLAPSED_FX: CpuWhy = CpuWhy::new("effects on a collapsed precomp");
pub const PRECOMP: CpuWhy = CpuWhy::new("a nested comp that needs the CPU (masks, styles or its contents)");
pub const THREE_D: CpuWhy = CpuWhy::new("3D layers the GPU can't draw together");

thread_local! {
    /// The reason the last plan on this thread declined (see [`declined`]).
    static DECLINED: Cell<Option<CpuWhy>> = const { Cell::new(None) };
}

/// A plan declined for `why`: kept for the caller ([`take`]). The first reason since the last
/// [`take`] wins (the innermost decline is the most specific).
pub fn declined(why: CpuWhy) {
    let _ = DECLINED.try_with(|d| {
        if d.get().is_none() {
            d.set(Some(why));
        }
    });
}

/// The reason left by [`declined`] since the last call, if any.
pub fn take() -> Option<CpuWhy> {
    DECLINED.try_with(Cell::take).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_names_its_effect_and_the_first_decline_wins() {
        assert_eq!(CpuWhy::named(EFFECT_NO_GPU.why, "Turbulent Displace").text(), "effect Turbulent Displace has no GPU version");
        assert_eq!(TAPER_STROKE.text(), "Taper or Wave stroke: the GPU can't draw it");
        let _ = take();
        declined(PATH_TOO_BIG);
        declined(TOO_MANY_EDGES);
        assert_eq!(take(), Some(PATH_TOO_BIG));
        assert_eq!(take(), None, "taking clears the slot");
    }
}
