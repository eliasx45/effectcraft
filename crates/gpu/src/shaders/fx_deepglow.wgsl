// GPU Deep Glow: see src/fx_deepglow.rs. Each kernel mirrors effects::misc::deep_glow's
// per-pixel steps operation for operation.

fn fdg_inside(p: vec2<i32>) -> bool {
    let d = out_dims();
    return p.x < d.x && p.y < d.y;
}

// misc::deep_glow_to_linear (Unmult: of the values as seen over black).
fn fdg_to_linear(px: vec4<f32>, unmult: bool) -> vec4<f32> {
    let a = px.w;
    if (unmult) {
        return vec4<f32>(srgb_to_linear(clamp(px.x, 0.0, 1.0)), srgb_to_linear(clamp(px.y, 0.0, 1.0)), srgb_to_linear(clamp(px.z, 0.0, 1.0)), a);
    }
    if (a > 0.0) {
        let c = clamp(px.xyz / a, vec3<f32>(0.0), vec3<f32>(1.0));
        return vec4<f32>(srgb_to_linear(c.x) * a, srgb_to_linear(c.y) * a, srgb_to_linear(c.z) * a, a);
    }
    return px;
}

// The glow input: misc::deep_glow_key, the Mask Layer (aux, misc::deep_glow_mask), then to linear.
// u[0] = (mask, luma mask, invert mask, to linear); u[1] = (unmult, src offset); f[0] = (threshold,
// smooth). The output is the padded frame; src is the unpadded layer at (offset, offset) in it.
@compute @workgroup_size(16, 16)
fn fdg_key(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let so = i32(P.u[1].y);
    let px = tex_get(src, p.x - so, p.y - so);
    let a = px.w;
    var l = 0.0;
    if (a > 0.0) {
        l = luminance(vec3<f32>(px.x / a, px.y / a, px.z / a));
    }
    let threshold = P.f[0].x;
    let soft = P.f[0].y;
    var k = 0.0;
    if (soft > 0.0) {
        k = clamp((l - threshold) / soft, 0.0, 1.0);
    } else if (l >= threshold) {
        k = 1.0;
    }
    var o = px * k;
    if (P.u[0].x != 0u) {
        let m = tex_get(aux, p.x, p.y);
        var amount = m.w;
        if (P.u[0].y != 0u && m.w > 0.0) {
            amount = luminance(vec3<f32>(m.x / m.w, m.y / m.w, m.z / m.w)) * m.w;
        }
        if (P.u[0].z != 0u) {
            amount = 1.0 - amount;
        }
        o = o * clamp(amount, 0.0, 1.0);
    }
    if (P.u[0].w != 0u) {
        o = fdg_to_linear(o, P.u[1].x != 0u);
    }
    textureStore(out, p, o);
}

// Two weighted glow passes summed: src · f[2].x · f[0] + aux · f[2].y · f[1].
@compute @workgroup_size(16, 16)
fn fdg_sum(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let a = textureLoad(src, p, 0) * P.f[2].x * P.f[0];
    let b = tex_get(aux, p.x, p.y) * P.f[2].y * P.f[1];
    textureStore(out, p, a + b);
}

// The final composite (the CPU's `finish`): the original (src, to linear) plus the exposed,
// tinted halo (aux), back to display values, Unmult's alpha, then Dither.
// u[0] = (to linear, unmult, screen, tint); u[1] = (tint mode, dither, monochrome, frame width);
// u[2] = (seed, src offset, halo offset); f[0] = (exposure, source opacity, tint mix, dither amount);
// f[1] = tint colour. src (the unpadded layer) and the halo (the frame's region that can glow) sit
// at their offsets in the padded output frame; outside them both are transparent.
@compute @workgroup_size(16, 16)
fn fdg_finish(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let convert = P.u[0].x != 0u;
    let unmult = P.u[0].y != 0u;
    let ho = i32(P.u[2].z);
    let so = i32(P.u[2].y);
    let glow = tex_get(aux, p.x - ho, p.y - ho);
    var original = tex_get(src, p.x - so, p.y - so);
    if (convert) {
        original = fdg_to_linear(original, unmult);
    }
    let exposure = P.f[0].x;
    let source_opacity = P.f[0].y;
    let tint_mix = P.f[0].z;
    var o = original;
    for (var c = 0; c < 3; c++) {
        var g = glow[c] * exposure;
        if (P.u[0].w != 0u) {
            let t = P.f[1][c];
            var tinted = g * t;
            if (P.u[1].x == 1u) {
                if (g <= 0.5) {
                    tinted = 2.0 * g * t;
                } else {
                    tinted = 1.0 - 2.0 * (1.0 - g) * (1.0 - t);
                }
            } else if (P.u[1].x == 2u) {
                if (t <= 0.5) {
                    tinted = g - (1.0 - 2.0 * t) * g * (1.0 - g);
                } else {
                    var d = sqrt(g);
                    if (g <= 0.25) {
                        d = ((16.0 * g - 12.0) * g + 4.0) * g;
                    }
                    tinted = g + (2.0 * t - 1.0) * (d - g);
                }
            }
            g += (tinted - g) * tint_mix;
        }
        let s = original[c] * source_opacity;
        if (P.u[0].z != 0u) {
            o[c] = clamp(s + g - s * g, 0.0, 1.0);
        } else {
            o[c] = s + g;
        }
    }
    if (unmult) {
        o.w = clamp(original.w * source_opacity, 0.0, 1.0);
    } else {
        o.w = clamp(glow.w * exposure + original.w * source_opacity, 0.0, 1.0);
    }
    if (convert) {
        let a = o.w;
        if (unmult) {
            o = vec4<f32>(linear_to_srgb(clamp(o.x, 0.0, 1.0)), linear_to_srgb(clamp(o.y, 0.0, 1.0)), linear_to_srgb(clamp(o.z, 0.0, 1.0)), a);
        } else if (a > 0.0) {
            for (var c = 0; c < 3; c++) {
                o[c] = min(linear_to_srgb(max(o[c] / a, 0.0)), 1.0) * a;
            }
        }
    }
    if (unmult) {
        o.w = clamp(max(o.w, max(o.x, max(o.y, o.z))), 0.0, 1.0);
    } else {
        o.w = 1.0;
    }
    if (P.u[1].y != 0u && o.w >= 0.999) {
        let x = u32(p.x);
        let y = u32(p.y);
        for (var c = 0u; c < 3u; c++) {
            var nx = x + c * P.u[1].w;
            if (P.u[1].z != 0u) {
                nx = x;
            }
            let n = (hash_noise(nx, y, P.u[2].x) - 0.5) * P.f[0].w;
            o[c] = max(o[c] + n, 0.0);
        }
    }
    textureStore(out, p, o);
}
