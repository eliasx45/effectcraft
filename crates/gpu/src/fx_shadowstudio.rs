//! GPU Shadow Studio 2 / 3 (`ec.perspective.shadowstudio2`, `ec.perspective.shadowstudio3`;
//! kernels in `shaders/fx_shadowstudio.wgsl`, every entry point prefixed `fss_`): the CPU
//! effect's steps (`effects::shadow_studio`) from the same plan (`shadow_studio_plan`: padding,
//! the cast's shifts and opacities, softness). The cast is one gather kernel (each padded pixel
//! keeps the most opaque step whose shifted source pixel is above the alpha threshold, which is
//! what the CPU's scatter over the source's bounding box leaves there), the softness is the
//! shared box-pass Gaussian, and the source over its shadow is one more kernel.

use effectcraft_effects::EffectCtx;

use crate::context::{Enc, Params};
use crate::effects::{GBuf, Precision, gaussian_blur_owned};

/// Compute entry points in `fx_shadowstudio.wgsl`.
pub(crate) const KERNELS: &[&str] = &["fss_cast", "fss_over"];

/// Effect ids implemented here.
pub(crate) const IDS: &[&str] = &["ec.perspective.shadowstudio2", "ec.perspective.shadowstudio3"];

/// Texel reads past which the cast runs on the CPU instead (one dispatch reading every step for
/// every padded pixel: a 4K frame at 512 samples would hold the GPU for seconds, and a driver
/// watchdog may reset it).
const CAST_BUDGET: u64 = 4_000_000_000;

/// Run effect `id` (one of [`IDS`]); `None` = this parameter combination runs on the CPU.
pub(crate) fn apply(e: &mut Enc, id: &str, ctx: &EffectCtx, b: GBuf) -> Option<GBuf> {
    match id {
        "ec.perspective.shadowstudio2" => shadow(e, ctx, b, false),
        "ec.perspective.shadowstudio3" => shadow(e, ctx, b, true),
        _ => None,
    }
}

fn shadow(e: &mut Enc, ctx: &EffectCtx, mut b: GBuf, v3: bool) -> Option<GBuf> {
    // No plan: the CPU effect leaves the buffer as it is.
    let Some(plan) = effectcraft_effects::shadow_studio_plan(ctx, b.img.width, b.img.height, b.scale, v3) else { return Some(b) };
    let [left, top, _, _] = plan.pad;
    let [w, h] = plan.size;
    if !e.g.fits(w, h) {
        return None;
    }
    let steps = u32::try_from(plan.steps.len()).ok()?;
    if (u64::from(w) * u64::from(h)).saturating_mul(u64::from(steps)) > CAST_BUDGET {
        return None;
    }
    // (x shift, y shift, opacity) per step; the shifts are whole pixels (|shift| ≤ 8192 · scale
    // at most, exact as f32).
    let mut data = Vec::with_capacity(plan.steps.len() * 3);
    for &(sx, sy, opacity) in &plan.steps {
        data.extend_from_slice(&[sx as f32, sy as f32, opacity]);
    }
    let data = e.data(&data);
    let mut p = Params::default();
    p.u[0] = [steps, left, top, 0];
    p.f[0] = plan.color;
    p.f[1] = [plan.alpha_threshold, 0.0, 0.0, 0.0];
    let cast = e.scratch(w, h);
    e.pixels("fss_cast", &p, &b.img, None, &cast, Some(&data));
    let cast = if plan.softness > 0.0 { gaussian_blur_owned(e, cast, plan.softness * 0.5, plan.softness * 0.5, false, Precision::F32) } else { cast };
    let mut q = Params::default();
    q.u[0] = [left, top, 0, 0];
    q.f[0] = [plan.source_opacity, 0.0, 0.0, 0.0];
    let out = e.scratch(w, h);
    e.pixels("fss_over", &q, &cast, Some(&b.img), &out, None);
    b.img = out;
    b.offset[0] += f64::from(left);
    b.offset[1] += f64::from(top);
    Some(b)
}
