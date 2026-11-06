// GPU Deep Glow: see src/fx_deepglow.rs. Each kernel mirrors effects::misc::deep_glow's
// per-pixel steps operation for operation.

fn fdg_inside(p: vec2<i32>) -> bool {
    let d = out_dims();
    return p.x < d.x && p.y < d.y;
}

// misc::DEEP_GLOW_GAMMA: the linear model's plain power, both ways (misc::dg_decode / dg_encode).
// With misc::DEEP_GLOW_TOE's straight line to black below linear 1e-5.
const FDG_GAMMA: f32 = 2.865;
const FDG_TOE: f32 = 1e-5;
fn fdg_decode(v0: f32) -> f32 {
    let knee = pow(FDG_TOE, 1.0 / FDG_GAMMA);
    let v = max(v0, 0.0);
    if (v < knee) {
        return v * FDG_TOE / knee;
    }
    return pow(v, FDG_GAMMA);
}
fn fdg_encode(v0: f32) -> f32 {
    let knee = pow(FDG_TOE, 1.0 / FDG_GAMMA);
    let v = max(v0, 0.0);
    if (v < FDG_TOE) {
        return v * knee / FDG_TOE;
    }
    return pow(v, 1.0 / FDG_GAMMA);
}

// misc::deep_glow_to_linear (Unmult: of the values as seen over black).
fn fdg_to_linear(px: vec4<f32>, unmult: bool) -> vec4<f32> {
    let a = px.w;
    if (unmult) {
        return vec4<f32>(fdg_decode(clamp(px.x, 0.0, 1.0)), fdg_decode(clamp(px.y, 0.0, 1.0)), fdg_decode(clamp(px.z, 0.0, 1.0)), a);
    }
    if (a > 0.0) {
        let c = clamp(px.xyz / a, vec3<f32>(0.0), vec3<f32>(1.0));
        return vec4<f32>(fdg_decode(c.x) * a, fdg_decode(c.y) * a, fdg_decode(c.z) * a, a);
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
            o = vec4<f32>(fdg_encode(clamp(o.x, 0.0, 1.0)), fdg_encode(clamp(o.y, 0.0, 1.0)), fdg_encode(clamp(o.z, 0.0, 1.0)), a);
        } else if (a > 0.0) {
            for (var c = 0; c < 3; c++) {
                o[c] = min(fdg_encode(max(o[c] / a, 0.0)), 1.0) * a;
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

// ---------------------------------------------------------------- Deep Glow 2 (effects::deepglow2)
// The Deep Glow 2 glow model: key → 13-tap downsample chain → one separable Gaussian per level
// → coarse-to-fine tent upsample → finish. Images are RGB (alpha unused, written 0) and are
// sampled GL_LINEAR clamp-to-edge in index space (deepglow2::lin / sample_row), as the CPU does.

fn fdg_bilinear(t: texture_2d<f32>, px: f32, py: f32) -> vec3<f32> {
    let x0 = floor(px);
    let y0 = floor(py);
    let fx = px - x0;
    let fy = py - y0;
    let xi = i32(x0);
    let yi = i32(y0);
    let a = tex_get_clamped(t, xi, yi).xyz;
    let b = tex_get_clamped(t, xi + 1, yi).xyz;
    let c = tex_get_clamped(t, xi, yi + 1).xyz;
    let d = tex_get_clamped(t, xi + 1, yi + 1).xyz;
    return (a * (1.0 - fx) + b * fx) * (1.0 - fy) + (c * (1.0 - fx) + d * fx) * fy;
}

fn fdg_smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// The glow input (deepglow2::dg2_key, then Screen's clamp and the gamma). src = the unpadded
// layer at (offset, offset) in the frame; aux = the fitted Mask Layer.
// u[0] = (mask, luma mask, invert mask, view input); u[1] = (src offset, screen); f[0] = (threshold,
// smooth, gamma). View input: the keyed, masked layer in display values with its alpha.
@compute @workgroup_size(16, 16)
fn fdg_dg2_key(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let so = i32(P.u[1].x);
    let px = tex_get(src, p.x - so, p.y - so);
    var x = max(px.xyz, vec3<f32>(0.0));
    let t = P.f[0].x;
    let s = P.f[0].y;
    if (t > 0.0) {
        x = vec3<f32>(x.x * fdg_smooth(t - s, t + s, x.x), x.y * fdg_smooth(t - s, t + s, x.y), x.z * fdg_smooth(t - s, t + s, x.z));
    }
    var amount = 1.0;
    if (P.u[0].x != 0u) {
        let m = tex_get(aux, p.x, p.y);
        amount = m.w;
        if (P.u[0].y != 0u && m.w > 0.0) {
            amount = luminance(vec3<f32>(m.x / m.w, m.y / m.w, m.z / m.w)) * m.w;
        }
        if (P.u[0].z != 0u) {
            amount = 1.0 - amount;
        }
        amount = clamp(amount, 0.0, 1.0);
    }
    x = x * amount;
    if (P.u[0].w != 0u) {
        textureStore(out, p, vec4<f32>(x, clamp(px.w * amount, 0.0, 1.0)));
        return;
    }
    if (P.u[1].y != 0u) {
        x = min(x, vec3<f32>(1.0));
    }
    let g = P.f[0].z;
    if (g > 1.0) {
        x = vec3<f32>(select(0.0, pow(x.x, g), x.x > 0.0), select(0.0, pow(x.y, g), x.y > 0.0), select(0.0, pow(x.z, g), x.z > 0.0));
    }
    textureStore(out, p, vec4<f32>(x, 0.0));
}

// The 13-tap downsample of src to out's size (offsets in destination texels).
// f[0] = (src w / out w, src h / out h).
@compute @workgroup_size(16, 16)
fn fdg_down(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let r = P.f[0].xy;
    let cx = (f32(p.x) + 0.5) * r.x - 0.5;
    let cy = (f32(p.y) + 0.5) * r.y - 0.5;
    var acc = 0.125 * fdg_bilinear(src, cx, cy);
    for (var j = -1; j <= 1; j += 2) {
        for (var i = -1; i <= 1; i += 2) {
            acc += 0.03125 * fdg_bilinear(src, cx + 2.0 * f32(i) * r.x, cy + 2.0 * f32(j) * r.y);
            acc += 0.125 * fdg_bilinear(src, cx + f32(i) * r.x, cy + f32(j) * r.y);
        }
    }
    acc += 0.0625 * fdg_bilinear(src, cx, cy + 2.0 * r.y);
    acc += 0.0625 * fdg_bilinear(src, cx, cy - 2.0 * r.y);
    acc += 0.0625 * fdg_bilinear(src, cx + 2.0 * r.x, cy);
    acc += 0.0625 * fdg_bilinear(src, cx - 2.0 * r.x, cy);
    textureStore(out, p, vec4<f32>(acc, 0.0));
}

// One axis of a level's Gaussian: 2·steps + 1 linear taps `stride` texels apart, weights
// deepglow2::dg2_weight × 1/sum, times the fade (f[1]; 1 on the vertical pass).
// u[0] = (vertical, steps); f[0] = (stride, 1 / weight sum).
@compute @workgroup_size(16, 16)
fn fdg_blur(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let vertical = P.u[0].x != 0u;
    let steps = i32(max(P.u[0].y, 1u));
    let stride = P.f[0].x;
    let inv = P.f[0].y;
    let d = vec2<i32>(textureDimensions(src));
    var acc = vec3<f32>(0.0);
    for (var k = -steps; k <= steps; k++) {
        let t = f32(k) / f32(steps);
        let w = exp(-0.5 * t * t / 0.06) * inv;
        let off = f32(k) * stride;
        var v: vec3<f32>;
        if (vertical) {
            let q = f32(p.y) + off;
            let i0 = floor(q);
            let f = q - i0;
            let a = clamp(i32(i0), 0, d.y - 1);
            let b = clamp(i32(i0) + 1, 0, d.y - 1);
            v = textureLoad(src, vec2<i32>(p.x, a), 0).xyz * (1.0 - f) + textureLoad(src, vec2<i32>(p.x, b), 0).xyz * f;
        } else {
            let q = f32(p.x) + off;
            let i0 = floor(q);
            let f = q - i0;
            let a = clamp(i32(i0), 0, d.x - 1);
            let b = clamp(i32(i0) + 1, 0, d.x - 1);
            v = textureLoad(src, vec2<i32>(a, p.y), 0).xyz * (1.0 - f) + textureLoad(src, vec2<i32>(b, p.y), 0).xyz * f;
        }
        acc += w * v;
    }
    textureStore(out, p, vec4<f32>(acc * P.f[1].xyz, 0.0));
}

// The finer level aux plus the 3 × 3 tent upsample of the coarser src (added, or screened).
// u[0] = (screen); f[0] = (src w / out w, src h / out h).
@compute @workgroup_size(16, 16)
fn fdg_up(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let r = P.f[0].xy;
    let cx = (f32(p.x) + 0.5) * r.x - 0.5;
    let cy = (f32(p.y) + 0.5) * r.y - 0.5;
    var u = vec3<f32>(0.0);
    for (var j = -1; j <= 1; j++) {
        for (var i = -1; i <= 1; i++) {
            let w = select(0.5, 0.25, i != 0) * select(0.5, 0.25, j != 0);
            u += w * fdg_bilinear(src, cx + f32(i) * r.x, cy + f32(j) * r.y);
        }
    }
    let d = textureLoad(aux, p, 0).xyz;
    var o = u + d;
    if (P.u[0].x != 0u) {
        o = u + d * (vec3<f32>(1.0) - u);
    }
    textureStore(out, p, vec4<f32>(o, 0.0));
}

// The finish (deepglow2::dg2_finish): the level-0 sum (aux) sampled to the frame, × exposure,
// tint, 1/gamma, Screen's clamp; the layer (src at (offset, offset)) on top; Unmult.
// u[0] = (unmult, screen, tint, tint mode); u[1] = (src offset); f[0] = (exposure, gamma,
// source opacity, tint mix); f[1] = tint colour; f[2] = (aux w / out w, aux h / out h).
@compute @workgroup_size(16, 16)
fn fdg_post(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!fdg_inside(p)) {
        return;
    }
    let so = i32(P.u[1].x);
    let px = tex_get(src, p.x - so, p.y - so);
    let r = P.f[2].xy;
    let g = fdg_bilinear(aux, (f32(p.x) + 0.5) * r.x - 0.5, (f32(p.y) + 0.5) * r.y - 0.5);
    let gamma = P.f[0].y;
    let inv = 1.0 / gamma;
    let screen = P.u[0].y != 0u;
    var o = vec4<f32>(0.0);
    for (var c = 0; c < 3; c++) {
        var v = g[c] * P.f[0].x;
        if (P.u[0].z != 0u) {
            let t = P.f[1][c];
            var tinted = v * t;
            if (P.u[0].w == 1u) {
                if (v <= 0.5) {
                    tinted = 2.0 * v * t;
                } else {
                    tinted = 1.0 - 2.0 * (1.0 - v) * (1.0 - t);
                }
            } else if (P.u[0].w == 2u) {
                if (t <= 0.5) {
                    tinted = v - (1.0 - 2.0 * t) * v * (1.0 - v);
                } else {
                    var dd = sqrt(v);
                    if (v <= 0.25) {
                        dd = ((16.0 * v - 12.0) * v + 4.0) * v;
                    }
                    tinted = v + (2.0 * t - 1.0) * (dd - v);
                }
            }
            v = v + (tinted - v) * P.f[0].w;
        }
        if (gamma > 1.0) {
            v = select(0.0, pow(v, inv), v > 0.0);
        }
        if (screen) {
            v = min(v, 1.0);
        }
        let s = px[c] * P.f[0].z;
        if (screen) {
            o[c] = 1.0 - (1.0 - v) * (1.0 - s);
        } else {
            o[c] = v + s;
        }
    }
    if (P.u[0].x != 0u) {
        o.w = clamp(max(o.x, max(o.y, o.z)), 0.0, 1.0);
    } else {
        o.w = 1.0;
    }
    textureStore(out, p, o);
}
