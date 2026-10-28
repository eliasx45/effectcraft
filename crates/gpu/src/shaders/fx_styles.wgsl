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
        // Softer: min(2 · blurred shape, 1); Precise (flag 32): the intensity itself.
        var v = min(textureLoad(src, p, 0).x * 2.0, 1.0);
        if ((P.u[0].w & 32u) != 0u) {
            v = textureLoad(src, p, 0).x;
        }
        let g = sty_glow_color(v, p);
        c = g.xyz;
        a = sty_noisy(g.w * P.f[0].w * (1.0 - textureLoad(aux, p, 0).w), P.f[1].z, p, seed ^ 0x5a5au);
    }
    a = clamp(a, 0.0, 1.0);
    textureStore(out, p, vec4<f32>(c * a, a));
}

// An interior style blended over the body inside the content (styles::blend_clipped).
// src = body; aux = (plane, 0, 0, content alpha) (overlays: the content itself).
// u[0] = (kind, mode, seed, flags: 1 gradient glow, 2 reverse, 4 invert, 8 centre, 16 dissolve,
// 32 Precise glow: aux.x is the intensity);
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
        var v = min(textureLoad(aux, p, 0).x * 2.0, 1.0);
        if ((flags & 32u) != 0u) {
            v = textureLoad(aux, p, 0).x;
        }
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

// ---------------------------------------------------------------- Bevel and Emboss
// styles::bevel on a distance field exact up to H = u[0].x pixels from the content edge (its
// profile and region are flat past the bevel size, so farther distances only need to stay
// >= H): squared distances are integers, computed in i32 on tiles of T = u[0].y pixels with
// H pixels of context on both sides (styles::edt is Felzenszwalb & Huttenlocher over the
// whole buffer; inside the window its minimum is the same number).

const BEV_T: i32 = 256;
const BEV_N: i32 = 768;

// Row pass: per row tile, the squared distance along the row to the nearest pixel with alpha
// >= 0.5 (x) and < 0.5 (y), capped at H²; w = alpha. Dispatched over (rows, tiles).
@compute @workgroup_size(64, 1)
fn sty_edt_rows(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let y = i32(gid.x);
    let x0 = i32(gid.y) * BEV_T;
    if (y >= dims.y || x0 >= dims.x) {
        return;
    }
    let hh = i32(P.u[0].x);
    let x1 = min(x0 + BEV_T, dims.x);
    var left_in: array<i32, 256>;
    var left_out: array<i32, 256>;
    var last_in = -1000000;
    var last_out = -1000000;
    for (var x = max(x0 - hh, 0); x < x1; x++) {
        if (textureLoad(src, vec2<i32>(x, y), 0).w >= 0.5) {
            last_in = x;
        } else {
            last_out = x;
        }
        if (x >= x0) {
            left_in[x - x0] = x - last_in;
            left_out[x - x0] = x - last_out;
        }
    }
    var next_in = 1000000;
    var next_out = 1000000;
    for (var x = min(x1 + hh, dims.x) - 1; x >= x0; x--) {
        let a = textureLoad(src, vec2<i32>(x, y), 0).w;
        if (a >= 0.5) {
            next_in = x;
        } else {
            next_out = x;
        }
        if (x < x1) {
            let di = min(min(left_in[x - x0], next_in - x), hh);
            let dout = min(min(left_out[x - x0], next_out - x), hh);
            textureStore(out, vec2<i32>(x, y), vec4<f32>(f32(di * di), f32(dout * dout), 0.0, a));
        }
    }
}

// Lower envelope of the parabolas f(i) + (q - i)² over a column window (styles::dt1d with
// the intersections kept as exact fractions zn / zd), written for the window's rows
// [o0, o1); returns nothing, stores channel `c` into `res`.
var<private> bev_f: array<i32, 768>;
var<private> bev_v: array<i32, 768>;
var<private> bev_zn: array<i32, 769>;
var<private> bev_zd: array<i32, 769>;
var<private> bev_res: array<vec2<i32>, 256>;

fn bev_envelope(n: i32, o0: i32, o1: i32, c: i32, cap: i32) {
    var k = 0;
    bev_v[0] = 0;
    for (var q = 1; q < n; q++) {
        let fq = bev_f[q] + q * q;
        var p = bev_v[k];
        var sn = fq - (bev_f[p] + p * p);
        var sd = 2 * (q - p);
        // s <= z[k] for k > 0 (both fractions with positive denominators).
        while (k > 0 && sn * bev_zd[k] <= bev_zn[k] * sd) {
            k -= 1;
            p = bev_v[k];
            sn = fq - (bev_f[p] + p * p);
            sd = 2 * (q - p);
        }
        k += 1;
        bev_v[k] = q;
        bev_zn[k] = sn;
        bev_zd[k] = sd;
    }
    let last = k;
    k = 0;
    for (var q = 0; q < o1; q++) {
        // z[k + 1] < q (z[last + 1] is +infinity).
        while (k < last && bev_zn[k + 1] < q * bev_zd[k + 1]) {
            k += 1;
        }
        if (q >= o0) {
            let dq = q - bev_v[k];
            let d = min(dq * dq + bev_f[bev_v[k]], cap);
            if (c == 0) {
                bev_res[q - o0].x = d;
            } else {
                bev_res[q - o0].y = d;
            }
        }
    }
}

// Column pass over the row pass (src): squared distances (x: to alpha >= 0.5, y: to < 0.5)
// exact below H², else H²; w = alpha. Dispatched over (columns, tiles).
@compute @workgroup_size(64, 1)
fn sty_edt_cols(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let x = i32(gid.x);
    let y0 = i32(gid.y) * BEV_T;
    if (x >= dims.x || y0 >= dims.y) {
        return;
    }
    let hh = i32(P.u[0].x);
    let cap = hh * hh;
    let y1 = min(y0 + BEV_T, dims.y);
    let w0 = max(y0 - hh, 0);
    let w1 = min(y1 + hh, dims.y);
    let n = w1 - w0;
    for (var c = 0; c < 2; c++) {
        for (var i = 0; i < n; i++) {
            let t = textureLoad(src, vec2<i32>(x, w0 + i), 0);
            bev_f[i] = i32(select(t.y, t.x, c == 0));
        }
        bev_envelope(n, y0 - w0, y1 - w0, c, cap);
    }
    for (var y = y0; y < y1; y++) {
        let r = bev_res[y - y0];
        textureStore(out, vec2<i32>(x, y), vec4<f32>(f32(r.x), f32(r.y), 0.0, textureLoad(src, vec2<i32>(x, y), 0).w));
    }
}

// styles::sdf from the column pass texel (x: to inside², y: to outside², w: alpha).
fn bev_sdf(t: vec4<f32>) -> f32 {
    let a = t.w;
    if (a > 0.0 && a < 1.0) {
        return 0.5 - a;
    }
    if (a >= 0.5) {
        return -(sqrt(t.y) - 0.5);
    }
    return sqrt(t.x) - 0.5;
}

// The bevel's height profile (styles::bevel). u[0].z = style, f[0].x = size.
@compute @workgroup_size(16, 16)
fn sty_bev_height(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let v = bev_sdf(textureLoad(src, p, 0));
    let size = P.f[0].x;
    var h = 0.0;
    switch P.u[0].z {
        case 0u: {
            h = clamp(1.0 - max(v, 0.0) / size, 0.0, 1.0);
        }
        case 2u, 4u: {
            h = clamp(0.5 - v / (2.0 * size), 0.0, 1.0);
        }
        case 3u: {
            h = clamp(abs(v) / size, 0.0, 1.0);
        }
        default: {
            h = clamp(-v / size + 0.5 / size, 0.0, 1.0);
        }
    }
    textureStore(out, p, vec4<f32>(h, 0.0, 0.0, 0.0));
}

// (smoothed height, region the bevel paints, 0, 0): src = smoothed height, aux = column pass.
@compute @workgroup_size(16, 16)
fn sty_bev_pack(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let t = textureLoad(aux, p, 0);
    let size = P.f[0].x;
    // styles::grown(sd, size).
    let grown = clamp(size + 0.5 - bev_sdf(t), 0.0, 1.0);
    var region = grown;
    if (P.u[0].z == 0u) {
        region = max(grown - t.w, 0.0);
    } else if (P.u[0].z == 1u) {
        region = t.w;
    }
    textureStore(out, p, vec4<f32>(textureLoad(src, p, 0).x, region, 0.0, 0.0));
}

fn bev_h(x: i32, y: i32) -> f32 {
    let d = vec2<i32>(textureDimensions(aux));
    return textureLoad(aux, vec2<i32>(clamp(x, 0, d.x - 1), clamp(y, 0, d.y - 1)), 0).x;
}

// Shadow, then highlight, blended onto the body (styles::bevel's two blend_into passes).
// src = body, aux = sty_bev_pack. u[0] = (0, shadow mode, seed, flags: 16 shadow dissolves,
// 32 highlight dissolves); u[1] = (org x, org y, highlight mode, 0); f[0] = (shadow colour,
// opacity); f[1] = (highlight colour, opacity); f[2] = (k, light x, light y, light z).
@compute @workgroup_size(16, 16)
fn sty_bevel(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    var d = textureLoad(src, p, 0);
    let region = textureLoad(aux, p, 0).y;
    let k = P.f[2].x;
    let light = P.f[2].yzw;
    let fl = light.z;
    let gx = (bev_h(p.x + 1, p.y) - bev_h(p.x - 1, p.y)) * 0.5 * k;
    let gy = (bev_h(p.x, p.y + 1) - bev_h(p.x, p.y - 1)) * 0.5 * k;
    let n = vec3<f32>(-gx, -gy, 1.0);
    let len = sqrt(n.x * n.x + n.y * n.y + 1.0);
    let dt = (n.x * light.x + n.y * light.y + n.z * light.z) / len;
    let hl = clamp((dt - fl) / max(1.0 - fl, 1e-4), 0.0, 1.0);
    let shd = clamp((fl - dt) / max(fl, 1e-4), 0.0, 1.0);
    let seed = P.u[0].z;
    let sa = shd * P.f[0].w * region;
    if (sa > 0.0) {
        var nz = 0.5;
        if ((P.u[0].w & 16u) != 0u) {
            nz = sty_noise(p, seed);
        }
        d = blend_pixel(P.u[0].y, d, vec4<f32>(P.f[0].xyz * sa, sa), nz);
    }
    let ha = hl * P.f[1].w * region;
    if (ha > 0.0) {
        var nz = 0.5;
        if ((P.u[0].w & 32u) != 0u) {
            nz = sty_noise(p, seed);
        }
        d = blend_pixel(P.u[1].z, d, vec4<f32>(P.f[1].xyz * ha, ha), nz);
    }
    textureStore(out, p, d);
}

// Image::alpha_bounds per row: texel y of the output = (first x with alpha > 1e-6, last x + 1,
// 0, 1), or zeros for an empty row. src = the image; dispatched over rows.
@compute @workgroup_size(64, 1)
fn sty_bounds(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = vec2<i32>(textureDimensions(src));
    let y = i32(gid.x);
    if (y >= d.y) {
        return;
    }
    var first = -1;
    var last = -1;
    for (var x = 0; x < d.x; x++) {
        if (textureLoad(src, vec2<i32>(x, y), 0).w > 1e-6) {
            if (first < 0) {
                first = x;
            }
            last = x;
        }
    }
    if (first < 0) {
        textureStore(out, vec2<i32>(y, 0), vec4<f32>(0.0));
    } else {
        textureStore(out, vec2<i32>(y, 0), vec4<f32>(f32(first), f32(last + 1), 0.0, 1.0));
    }
}

// ---------------------------------------------------------------- Distance-field shapes (GLS1)
// The styles that read the content's distance field (the column pass above, exact within the
// farthest of their radii) besides Bevel and Emboss.

// styles::grown: coverage of the content grown by r (negative r shrinks).
fn sty_grown(sd: f32, r: f32) -> f32 {
    return clamp(r + 0.5 - sd, 0.0, 1.0);
}

// A shape from the field (src = column pass): (outer, inner, 0, alpha), inner reading the
// negated field. u[0].z = 0: the content grown by f[0].x (styles::soft_shape's Spread / Choke
// base); 1: a Precise glow's intensity, hard part f[0].x, falloff f[0].y
// (styles::glow_intensity).
@compute @workgroup_size(16, 16)
fn sty_field_shape(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    let t = textureLoad(src, p, 0);
    let sd = bev_sdf(t);
    let r = P.f[0].x;
    var o = 0.0;
    var i = 0.0;
    if (P.u[0].z == 1u) {
        let soft = P.f[0].y;
        o = clamp(1.0 - max(sd - r, 0.0) / soft, 0.0, 1.0);
        i = clamp(1.0 - max(-sd - r, 0.0) / soft, 0.0, 1.0);
    } else {
        o = sty_grown(sd, r);
        i = sty_grown(-sd, r);
    }
    textureStore(out, p, vec4<f32>(o, i, 0.0, t.w));
}

// Stroke over the body (styles::render's Stroke step). src = body, aux = column pass (w: the
// content alpha). u[0] = (0, mode, seed, flags: 16 dissolve); u[1].xy = org;
// f[0] = (colour, opacity); f[1] = (inside width, outside width, 0, 0).
@compute @workgroup_size(16, 16)
fn sty_stroke(@builtin(global_invocation_id) gid: vec3<u32>) {
    let p = vec2<i32>(gid.xy);
    if (!sty_inside(p)) {
        return;
    }
    var d = textureLoad(src, p, 0);
    let t = textureLoad(aux, p, 0);
    let a = t.w;
    let sd = bev_sdf(t);
    let c = P.f[0].xyz;
    let r_in = P.f[1].x;
    let r_out = P.f[1].y;
    // Inside: blended over the body within the content (styles::blend_clipped), over the
    // share of the pixel's coverage the ring takes.
    if (r_in > 0.0 && a > 0.0) {
        let ring = clamp((a - sty_grown(sd, -r_in)) / max(a, 1e-6), 0.0, 1.0);
        let sa = ring * P.f[0].w;
        if (sa > 0.0) {
            var n = 0.5;
            if ((P.u[0].w & 16u) != 0u) {
                n = sty_noise(p, P.u[0].z);
            }
            d = blend_pixel(P.u[0].y, d / a, vec4<f32>(c * sa, sa), n) * a;
        }
    }
    // Outside: covers exactly what the content leaves uncovered (added, premultiplied).
    if (r_out > 0.0) {
        let ring = max(sty_grown(sd, r_out) - a, 0.0) * P.f[0].w;
        if (ring > 0.0) {
            d = d + vec4<f32>(c * ring, ring);
        }
    }
    textureStore(out, p, d);
}
