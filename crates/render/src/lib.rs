//! The EffectCraft compositor (CPU reference path).
//!
//! `Renderer::comp_frame` renders a composition at a time:
//! bottom-to-top over visible layers → **source** (solid, footage, text, shapes, precomp) →
//! **masks** → **effects** → **transform** (2D affine or 3D projective through the active camera,
//! with motion blur sub-samples) → **track matte** → **blend** with the layer's mode and opacity.
//! Adjustment layers run their effects on everything below, limited by their own bounds/masks.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// Every `src/*.rs` file is a module (`tests*.rs` under `#[cfg(test)]`), declared by build.rs (docs/architecture.md,
// "Self-registering modules"); declare one here only for another visibility or a submodule folder.
include!(concat!(env!("OUT_DIR"), "/modules.rs"));
pub mod cache;
mod clip;
pub mod masks;
pub mod shapes;
pub mod text;
pub mod yuv;

// The compositor is split by topic: `renderer` (the `Renderer`, its options, the comp walk),
// `sources` and `footage` (layer sources), `layer_buf` (source → masks → effects → styles,
// cached), `fx` (the effect stack), `placement` and `motion_blur` (where a buffer lands),
// `composite` and `adjustment` (blending into the comp), `collapse` (Collapse
// Transformations) and `accel` (hooks for accelerated compositors).

// The tests name `crate::Buf`.
#[cfg(test)]
use effectcraft_effects::Buf;

pub use accel::{Accelerator, Backend, FxStep, FxTarget};
pub use adjustment::adjustment_crop;
pub use auto::{AutoKey, AutoPick};
pub use cache::{CacheStats, LayerCache, LayerStore, PrefetchStore};
pub use clip::{set_precomp_clip, set_shape_clip};
pub use cpu_why::CpuWhy;
pub use effectcraft_raster::Image;
pub use eval::{EvalCtx, ExprHost, effect_bounds, source_size};
pub use footage::{
    CancellableFootage, FloatCurve, FootageCacheUsage, FootageFrame, FootageSource, FramePixels, NoFootage, SharedFrame, blend_frames, exact_footage,
    field_frame, frame_position, interpret_pixels, is_vector_footage, preview_size,
};
pub(crate) use fx::comp_scene;
pub use fx::{effect_canvas_extent, effect_canvas_extent_in, effect_points_in_comp_space, effect_space_follows_comp_size};
pub use layer_buf::{ContentClip, StyledLayer};
pub use motion_blur::adaptive_samples;
pub use placement::Placement;
pub use renderer::{LayerTimes, LayerTiming, RenderOpts, Renderer, SHARP_MAX_PIXELS, TakenTimes, extended_region, render_frame, roi_inside};
pub use sources::{content_bounds, essential_overrides, kurbo_path};
