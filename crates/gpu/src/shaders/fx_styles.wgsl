// GPU Layer Styles: see src/fx_styles.rs. Each kernel mirrors a step of render::styles::render
// operation for operation; `data` holds packed gradient stops (styles::pack_gradient).

fn sty_inside(p: vec2<i32>) -> bool {
    let d = out_dims();
    return p.x < d.x && p.y < d.y;
}

// styles::Org::noise: the uncropped buffer's coordinates (org in P.u[1].xy).
fn sty_noise(p: vec2<i32>, seed: u32) -> f32 {
    return hash_noise(u32(p.x) + P.u[1].x, u32(p.y) + P.u[1].y, seed);
}

// styles::noisy.
fn sty_noisy(a: f32, noise: f32, p: vec2<i32>, seed: u32) -> f32 {
    if (noise <= 0.0) {
        return a;
    }
    return a * (1.0 - noise * sty_noise(p, seed));
}

// Gradient::sample over the stops at data[0..].
fn sty_grad(t0: f32) -> vec4<f32> {
    let t = clamp(t0, 0.0, 1.0);
    let nc = u32(data[0]);
    let no = u32(data[1]);
    var col = vec4<f32>(1.0);
    if (nc > 0u) {
        let first = 2u;
        col = vec4<f32>(data[first + 1u], data[first + 2u], data[first + 3u], data[first + 4u]);
        if (t > data[first]) {
            var found = false;
            for (var i = 0u; i + 1u < nc; i = i + 1u) {
                let a = 2u + i * 6u;
                let b = a + 6u;
                if (t <= data[b]) {
                    let span = max(data[b] - data[a], 1e-12);
                    var f = clamp((t - data[a]) / span, 0.0, 1.0);
                    let m = data[a + 5u];
                    if (f <= m) {
                        f = 0.5 * f / m;
                    } else {
                        f = 0.5 + 0.5 * (f - m) / (1.0 - m);
                    }
                    let ca = vec4<f32>(data[a + 1u], data[a + 2u], data[a + 3u], data[a + 4u]);
                    let cb = vec4<f32>(data[b + 1u], data[b + 2u], data[b + 3u], data[b + 4u]);
                    col = ca + (cb - ca) * f;
                    found = true;
                    break;
                }
            }
            if (!found) {
                let l = 2u + (nc - 1u) * 6u;
                col = vec4<f32>(data[l + 1u], data[l + 2u], data[l + 3u], data[l + 4u]);
            }
        }
    }
    var op = 1.0;
    if (no > 0u) {
        let base = 2u + nc * 6u;
        op = data[base + 1u];
        if (t > data[base]) {
            var found = false;
            for (var i = 0u; i + 1u < no; i = i + 1u) {
                let a = base + i * 3u;
                let b = a + 3u;
                if (t <= data[b]) {
                    let span = max(data[b] - data[a], 1e-12);
                    var f = clamp((t - data[a]) / span, 0.0, 1.0);
                    let m = data[a + 2u];
                    if (f <= m) {
                        f = 0.5 * f / m;
                    } else {
                        f = 0.5 + 0.5 * (f - m) / (1.0 - m);
                    }
                    op = data[a + 1u] + (data[b + 1u] - data[a + 1u]) * f;
                    found = true;
                    break;
                }
            }
            if (!found) {
                op = data[base + (no - 1u) * 3u + 1u];
            }
        }
    }
    return vec4<f32>(col.xyz, col.w * op);
}

// styles::glow_color: (colour, alpha) of a glow intensity. f[0].xyz colour, f[2].x range,
// f[1].w jitter, u[0].w bit 0 gradient.
fn sty_glow_color(v0: f32, p: vec2<i32>) -> vec4<f32> {
    let v = powz(clamp(v0, 0.0, 1.0), 0.5 / P.f[2].x);
    if ((P.u[0].w & 1u) == 0u) {
        return vec4<f32>(P.f[0].xyz, v);
    }
    var j = 0.0;
    if (P.f[1].w > 0.0) {
        j = (sty_noise(p, 77u) - 0.5) * P.f[1].w;
    }
    let c = sty_grad(1.0 - v + j);
    return vec4<f32>(c.xyz, c.w * min(v * 8.0, 1.0));
}

// The planes styles blur: (alpha, 1 − alpha, 0, alpha) of the content.
@compute @workgroup_size(16, 16)
fn sty_alpha(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let a = textureLoad(src, p, 0).w;
    textureStore(out, p, vec4<f32>(a, 1.0 - a, 0.0, a));
}

// A plane (src.x, or src.y with u[0].x = 1) next to the content alpha (aux.w): (plane, 0, 0, alpha).
@compute @workgroup_size(16, 16)
fn sty_pack(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let s = textureLoad(src, p, 0);
    let v = select(s.x, s.y, P.u[0].x == 1u);
    textureStore(out, p, vec4<f32>(v, 0.0, 0.0, textureLoad(aux, p, 0).w));
}

// Fill Opacity: every channel × f[0].x (Image::scale_alpha).
@compute @workgroup_size(16, 16)
fn sty_scale(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    textureStore(out, p, textureLoad(src, p, 0) * P.f[0].x);
}

// An exterior pass (styles::pass_image). src = blurred plane, aux = content.
// u[0] = (kind: 0 drop shadow / 1 outer glow, seed, knocks out, flags); u[1].xy = org;
// f[0] = (colour, opacity); f[1] = (offset x, offset y, noise, jitter); f[2].x = range.
@compute @workgroup_size(16, 16)
fn sty_pass(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let seed = P.u[0].y;
    var c = P.f[0].xyz;
    var a = 0.0;
    if (P.u[0].x == 0u) {
        a = sample_bilinear_clamped(src, f32(p.x) + 0.5 - P.f[1].x, f32(p.y) + 0.5 - P.f[1].y).x * P.f[0].w;
        if (P.u[0].z != 0u) {
            a = a * (1.0 - textureLoad(aux, p, 0).w);
        }
        a = sty_noisy(a, P.f[1].z, p, seed);
    } else {
        let v = min(textureLoad(src, p, 0).x * 2.0, 1.0);
        let g = sty_glow_color(v, p);
        c = g.xyz;
        a = sty_noisy(g.w * P.f[0].w * (1.0 - textureLoad(aux, p, 0).w), P.f[1].z, p, seed ^ 0x5a5au);
    }
    a = clamp(a, 0.0, 1.0);
    textureStore(out, p, vec4<f32>(c * a, a));
}

// An interior style blended over the body inside the content (styles::blend_clipped).
// src = body; aux = (plane, 0, 0, content alpha) (overlays: the content itself).
// u[0] = (kind, mode, seed, flags: 1 gradient glow, 2 reverse, 4 invert, 8 centre, 16 dissolve);
// u[1] = (org x, org y, gradient style, 0); f[0] = (colour, opacity);
// f[1] = (offset x, offset y, noise, jitter); f[2] = (range, centre x, centre y, extent);
// f[3] = (dir x, dir y, radius, angle). Kinds: 0 colour overlay, 1 gradient overlay, 2 satin,
// 3 inner glow, 4 inner shadow.
@compute @workgroup_size(16, 16)
fn sty_body(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let d = textureLoad(src, p, 0);
    let k = textureLoad(aux, p, 0).w;
    if (k <= 0.0) {
        textureStore(out, p, d);
        return;
    }
    let kind = P.u[0].x;
    let seed = P.u[0].z;
    let flags = P.u[0].w;
    let x = f32(p.x) + 0.5;
    let y = f32(p.y) + 0.5;
    var c = P.f[0].xyz;
    var a = P.f[0].w;
    if (kind == 1u) {
        let px = x - P.f[2].y;
        let py = y - P.f[2].z;
        let ux = P.f[3].x;
        let uy = P.f[3].y;
        let along = px * ux + py * uy;
        let across = -px * uy + py * ux;
        let ext = P.f[2].w;
        var t = 0.5 + along / (2.0 * ext);
        switch P.u[1].z {
            case 1u: {
                t = sqrt(px * px + py * py) / P.f[3].z;
            }
            case 2u: {
                let an = atan2(-py, px) - P.f[3].w;
                t = fract_euclid(an / 6.283185307179586);
            }
            case 3u: {
                t = abs(along) / ext;
            }
            case 4u: {
                t = (abs(along) + abs(across)) / ext;
            }
            default: {}
        }
        if ((flags & 2u) != 0u) {
            t = 1.0 - t;
        }
        let g = sty_grad(clamp(t, 0.0, 1.0));
        c = g.xyz;
        a = g.w * P.f[0].w;
    } else if (kind == 2u) {
        let o = P.f[1].xy;
        let s1 = sample_bilinear_clamped(aux, x - o.x, y - o.y).x;
        let s2 = sample_bilinear_clamped(aux, x + o.x, y + o.y).x;
        var v = min(abs(s1 - s2), 1.0);
        if ((flags & 4u) != 0u) {
            v = 1.0 - v;
        }
        a = v * P.f[0].w;
    } else if (kind == 3u) {
        let v = min(textureLoad(aux, p, 0).x * 2.0, 1.0);
        let e = select(v, 1.0 - v, (flags & 8u) != 0u);
        let g = sty_glow_color(e, p);
        c = g.xyz;
        a = sty_noisy(g.w * P.f[0].w, P.f[1].z, p, seed ^ 0x3c3cu);
    } else if (kind == 4u) {
        let o = P.f[1].xy;
        a = min(sample_bilinear_clamped(aux, x - o.x, y - o.y).x, 1.0) * P.f[0].w;
        a = sty_noisy(a, P.f[1].z, p, seed ^ 0x1234u);
    }
    if (a <= 0.0) {
        textureStore(out, p, d);
        return;
    }
    var n = 0.5;
    if ((flags & 16u) != 0u) {
        n = sty_noise(p, seed);
    }
    let b = blend_pixel(P.u[0].y, d / k, vec4<f32>(c * a, a), n);
    textureStore(out, p, b * k);
}
