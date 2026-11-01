// Compositing and effect kernels (rgba32float output). Parameter layouts are documented next to
// each entry point and mirrored by the Rust dispatch code (src/ops.rs, src/effects.rs).

@group(0) @binding(3) var out: texture_storage_2d<rgba32float, write>;

fn out_dims() -> vec2<i32> {
    return vec2<i32>(textureDimensions(out));
}

// ---------------------------------------------------------------- compositing

// Warp `src` by the inverse matrix (output pixel → source pixel) and blend over `aux`
// (raster::composite_warp / accumulate_warp).
// u[0] = (mode, sampling, seed, flags: 1 accumulate, 2 clear outside bounds)
// f[0..3] = inverse rows; f[3] = (opacity, weight); f[4] = bounds (x0, y0, x1, y1)
@compute @workgroup_size(16, 16)
fn warp_blend(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= dims.x || y >= dims.y) {
        return;
    }
    let d = textureLoad(aux, vec2<i32>(x, y), 0);
    store_q(vec2<i32>(x, y), warp_blend_px(x, y, d), P.f[3].z, P.f[3].w);
}

// `warp_blend` in place, over the canvas rectangle u[1] = (x0, y0, x1, y1) only (the layer's
// bounds): `out` is the canvas itself, `aux` that rectangle of it (copied out first: a pass
// can't read and write one texture), and the pixels outside it stay as they are.
@compute @workgroup_size(16, 16)
fn warp_blend_at(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let o = P.u[1];
    let x = i32(o.x + gid.x);
    let y = i32(o.y + gid.y);
    if (x >= i32(o.z) || y >= i32(o.w) || x >= dims.x || y >= dims.y) {
        return;
    }
    let d = textureLoad(aux, vec2<i32>(gid.xy), 0);
    store_q(vec2<i32>(x, y), warp_blend_px(x, y, d), P.f[3].z, P.f[3].w);
}

// One `warp_blend` pixel: the value stored at (x, y) over the canvas pixel `d0`.
fn warp_blend_px(x: i32, y: i32, d0: vec4<f32>) -> vec4<f32> {
    let mode = P.u[0].x;
    let sampling = P.u[0].y;
    let seed = P.u[0].z;
    let flags = P.u[0].w;
    let b = P.f[4];
    var d = d0;
    let fx = f32(x) + 0.5;
    let fy = f32(y) + 0.5;
    if (fx < b.x || fy < b.y || fx > b.z || fy > b.w) {
        if ((flags & 2u) != 0u) {
            d = vec4<f32>(0.0);
        }
        return d;
    }
    let r0 = P.f[0];
    let r1 = P.f[1];
    let r2 = P.f[2];
    var px = r0.x * fx + r0.y * fy + r0.z;
    var py = r1.x * fx + r1.y * fy + r1.z;
    let w = r2.x * fx + r2.y * fy + r2.z;
    if (w != 1.0 && w != 0.0) {
        px = px / w;
        py = py / w;
    }
    let sz = vec2<f32>(textureDimensions(src));
    let stencil = is_stencil(mode);
    if (px < -1.0 || py < -1.0 || px > sz.x + 1.0 || py > sz.y + 1.0) {
        if (stencil && (flags & 1u) == 0u) {
            d = blend_pixel(mode, d, vec4<f32>(0.0), 0.5);
        }
        return d;
    }
    var s = sample(src, sampling, px, py);
    if ((flags & 1u) != 0u) {
        if (s.w > 0.0) {
            d = d + s * P.f[3].y;
        }
        return d;
    }
    if (s.w <= 0.0 && !stencil) {
        return d;
    }
    let op = P.f[3].x;
    if (op < 1.0) {
        s = s * op;
    }
    var n = 0.5;
    if (mode == 1u || mode == 2u) {
        n = hash_noise(u32(x), u32(y), seed);
    }
    return blend_pixel(mode, d, s, n);
}

// Motion-blur sub-samples of one source in one pass: `aux` plus each sample's warp of `src`
// times the weight, summed in sample order exactly as consecutive `warp_blend` accumulate
// passes would (the same per-sample bounds and source-range tests).
// u[0] = (sampling, sample count); f[0] = (weight, union bounds x0, y0, x1), f[1].x = union y1;
// data: per sample 16 floats = inverse rows r0, r1, r2 (xyz_) and bounds (x0, y0, x1, y1).
@compute @workgroup_size(16, 16)
fn warp_accum(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let x = i32(gid.x);
    let y = i32(gid.y);
    if (x >= dims.x || y >= dims.y) {
        return;
    }
    textureStore(out, vec2<i32>(x, y), warp_accum_px(x, y, textureLoad(aux, vec2<i32>(x, y), 0)));
}

// `warp_accum` in place over u[1] = (x0, y0, x1, y1) (as `warp_blend_at`).
@compute @workgroup_size(16, 16)
fn warp_accum_at(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let o = P.u[1];
    let x = i32(o.x + gid.x);
    let y = i32(o.y + gid.y);
    if (x >= i32(o.z) || y >= i32(o.w) || x >= dims.x || y >= dims.y) {
        return;
    }
    textureStore(out, vec2<i32>(x, y), warp_accum_px(x, y, textureLoad(aux, vec2<i32>(gid.xy), 0)));
}

// One `warp_accum` pixel: the sum at (x, y) over the canvas pixel `d0`.
fn warp_accum_px(x: i32, y: i32, d0: vec4<f32>) -> vec4<f32> {
    let sampling = P.u[0].x;
    let count = P.u[0].y;
    let k = P.f[0].x;
    var d = d0;
    let fx = f32(x) + 0.5;
    let fy = f32(y) + 0.5;
    if (fx < P.f[0].y || fy < P.f[0].z || fx > P.f[0].w || fy > P.f[1].x) {
        return d;
    }
    let sz = vec2<f32>(textureDimensions(src));
    for (var i = 0u; i < count; i = i + 1u) {
        let o = i * 16u;
        if (fx < data[o + 12u] || fy < data[o + 13u] || fx > data[o + 14u] || fy > data[o + 15u]) {
            continue;
        }
        var px = data[o] * fx + data[o + 1u] * fy + data[o + 2u];
        var py = data[o + 4u] * fx + data[o + 5u] * fy + data[o + 6u];
        let w = data[o + 8u] * fx + data[o + 9u] * fy + data[o + 10u];
        if (w != 1.0 && w != 0.0) {
            px = px / w;
            py = py / w;
        }
        if (px < -1.0 || py < -1.0 || px > sz.x + 1.0 || py > sz.y + 1.0) {
            continue;
        }
        let s = sample(src, sampling, px, py);
        if (s.w > 0.0) {
            d = d + s * k;
        }
    }
    return d;
}

// Blend `src` (same size) onto `aux` (Image::blend_from).
// u[0] = (mode, _, seed); f[0].x = opacity
@compute @workgroup_size(16, 16)
fn blend_full(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    store_q(p, blend_full_px(p, textureLoad(aux, p, 0)), P.f[0].y, P.f[0].z);
}

// `blend_full` in place over u[1] = (x0, y0, x1, y1): `out` is the canvas, `aux` that
// rectangle of it (as `warp_blend_at`), `src` the layer.
@compute @workgroup_size(16, 16)
fn blend_full_at(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let o = P.u[1];
    let p = vec2<i32>(i32(o.x + gid.x), i32(o.y + gid.y));
    if (p.x >= i32(o.z) || p.y >= i32(o.w) || p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    store_q(p, blend_full_px(p, textureLoad(aux, vec2<i32>(gid.xy), 0)), P.f[0].y, P.f[0].z);
}

// One `blend_full` pixel: `src` at `p` blended over the canvas pixel `d` (before quantising).
fn blend_full_px(p: vec2<i32>, d: vec4<f32>) -> vec4<f32> {
    let mode = P.u[0].x;
    var s = textureLoad(src, p, 0);
    if (P.u[0].y != 0u) { s = s * d.w; }
    if (s.w <= 0.0 && !is_stencil(mode)) {
        return d;
    }
    s = s * P.f[0].x;
    var n = 0.5;
    if (mode == 1u || mode == 2u) {
        n = hash_noise(u32(p.x), u32(p.y), P.u[0].z);
    }
    return blend_pixel(mode, d, s, n);
}

// Track matte: `src` × matte factor of `aux`. u[0].x = 0 alpha, 1 alpha inverted, 2 luma,
// 3 luma inverted.
@compute @workgroup_size(16, 16)
fn matte(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, matte_px(p, textureLoad(src, p, 0)));
}

// `matte` in place over u[1] = (x0, y0, x1, y1): `out` is the layer, `src` that rectangle of
// it, `aux` the matte.
@compute @workgroup_size(16, 16)
fn matte_at(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let o = P.u[1];
    let p = vec2<i32>(i32(o.x + gid.x), i32(o.y + gid.y));
    if (p.x >= i32(o.z) || p.y >= i32(o.w) || p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, matte_px(p, textureLoad(src, vec2<i32>(gid.xy), 0)));
}

// One `matte` pixel: the layer pixel `s` × the matte factor at `p`.
fn matte_px(p: vec2<i32>, s: vec4<f32>) -> vec4<f32> {
    let q = textureLoad(aux, p, 0);
    var k: f32;
    switch P.u[0].x {
        case 0u: { k = q.w; }
        case 1u: { k = 1.0 - q.w; }
        case 2u: { k = luminance(q.xyz); }
        default: { k = 1.0 - luminance(q.xyz); }
    }
    k = clamp(k, 0.0, 1.0);
    return s * k;
}

// Preserve Transparency: `src` × alpha of `aux`.
@compute @workgroup_size(16, 16)
fn preserve(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, textureLoad(src, p, 0) * textureLoad(aux, p, 0).w);
}

// Knockout: `src` × (1 − alpha of `aux`).
@compute @workgroup_size(16, 16)
fn knockout(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let keep = 1.0 - clamp(textureLoad(aux, p, 0).w, 0.0, 1.0);
    textureStore(out, p, textureLoad(src, p, 0) * keep);
}

// Styled layer finish: canvas `src` moves toward `aux` by the opacity, per enabled channel.
// u[0].xyz = R/G/B switches; f[0].x = opacity.
@compute @workgroup_size(16, 16)
fn channel_mix(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, channel_mix_px(p, textureLoad(src, p, 0)));
}

// `channel_mix` in place over u[1] = (x0, y0, x1, y1): `out` is the canvas, `src` that
// rectangle of it, `aux` the styled layer's result.
@compute @workgroup_size(16, 16)
fn channel_mix_at(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let o = P.u[1];
    let p = vec2<i32>(i32(o.x + gid.x), i32(o.y + gid.y));
    if (p.x >= i32(o.z) || p.y >= i32(o.w) || p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, channel_mix_px(p, textureLoad(src, vec2<i32>(gid.xy), 0)));
}

// One `channel_mix` pixel: the canvas pixel `c` moved toward `aux` at `p`.
fn channel_mix_px(p: vec2<i32>, c: vec4<f32>) -> vec4<f32> {
    let t = textureLoad(aux, p, 0);
    let op = P.f[0].x;
    var v = t;
    if (P.u[0].x == 0u) {
        v.x = c.x;
    }
    if (P.u[0].y == 0u) {
        v.y = c.y;
    }
    if (P.u[0].z == 0u) {
        v.z = c.z;
    }
    return c + (v - c) * op;
}

// 8/16 bpc: clamp premultiplied pixels to 0..1 (colour ≤ alpha) and round. f[0].x = levels.
@compute @workgroup_size(16, 16)
fn quantize(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, quantize_px(textureLoad(src, p, 0), P.f[0].x, P.f[0].y));
}

fn quantize_px(s: vec4<f32>, levels: f32, inv: f32) -> vec4<f32> {
    // Use the CPU-rounded reciprocal to avoid backend-dependent division.
    let a = floor(clamp(s.w, 0.0, 1.0) * levels + 0.5) * inv;
    var o = vec4<f32>(0.0, 0.0, 0.0, a);
    o.x = floor(clamp(s.x, 0.0, a) * levels + 0.5) * inv;
    o.y = floor(clamp(s.y, 0.0, a) * levels + 0.5) * inv;
    o.z = floor(clamp(s.z, 0.0, a) * levels + 0.5) * inv;
    return o;
}

// Store a compositing result, clamped and quantised to `levels` when > 0 (8/16 bpc: the comp
// after a layer, fused into the layer's composite).
fn store_q(p: vec2<i32>, v: vec4<f32>, levels: f32, inv: f32) {
    if (levels > 0.0) {
        textureStore(out, p, quantize_px(v, levels, inv));
    } else {
        textureStore(out, p, v);
    }
}

// Colour conversion on straight colour (render::color::convert).
// u[0] = (decode curve, has matrix, encode curve); f[0..3] = matrix rows.
@compute @workgroup_size(16, 16)
fn convert(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let s = textureLoad(src, p, 0);
    let a = s.w;
    if (a <= 0.0) {
        textureStore(out, p, s);
        return;
    }
    var c = s.xyz / a;
    let dc = P.u[0].x;
    if (dc != 0u) {
        c = vec3<f32>(curve_decode(dc, c.x), curve_decode(dc, c.y), curve_decode(dc, c.z));
    }
    if (P.u[0].y != 0u) {
        let m0 = P.f[0].xyz;
        let m1 = P.f[1].xyz;
        let m2 = P.f[2].xyz;
        c = vec3<f32>(m0.x * c.x + m0.y * c.y + m0.z * c.z, m1.x * c.x + m1.y * c.y + m1.z * c.z, m2.x * c.x + m2.y * c.y + m2.z * c.z);
    }
    let ec = P.u[0].z;
    if (ec != 0u) {
        c = vec3<f32>(curve_encode(ec, c.x), curve_encode(ec, c.y), curve_encode(ec, c.z));
    }
    textureStore(out, p, vec4<f32>(c * a, a));
}

// Box-filter halving for minification (raster::warp::half).
@compute @workgroup_size(16, 16)
fn half(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let sx = 2 * p.x;
    let sy = 2 * p.y;
    var acc = vec4<f32>(0.0);
    acc += tex_get_clamped(src, sx, sy);
    acc += tex_get_clamped(src, sx + 1, sy);
    acc += tex_get_clamped(src, sx, sy + 1);
    acc += tex_get_clamped(src, sx + 1, sy + 1);
    textureStore(out, p, acc * 0.25);
}

// ---------------------------------------------------------------- blurs (raster::blur)

fn line_get(horizontal: bool, line: i32, i: i32, n: i32, repeat: bool) -> vec4<f32> {
    var j = i;
    if (i < 0 || i >= n) {
        if (!repeat) {
            return vec4<f32>(0.0);
        }
        j = clamp(i, 0, n - 1);
    }
    if (horizontal) {
        return textureLoad(src, vec2<i32>(j, line), 0);
    }
    return textureLoad(src, vec2<i32>(line, j), 0);
}

// One box pass of radius r along rows (horizontal) or columns, as running sums over blocks of
// `block` pixels (each block restarts its sum, as the CPU's vertical pass does). u[0].w = the
// blocks' phase: they start `phase` pixels before the image (an adjustment layer's window of
// the comp keeps the comp's block grid, `adjust_window`); 0 for a whole image.
fn box_pass(gid: vec3<u32>, horizontal: bool) {
    let dims = out_dims();
    let r = i32(P.u[0].x);
    let repeat = P.u[0].y != 0u;
    let block = i32(P.u[0].z);
    let phase = i32(P.u[0].w);
    var lines = dims.x;
    var n = dims.y;
    if (horizontal) {
        lines = dims.y;
        n = dims.x;
    }
    let line = i32(gid.x);
    let s0 = i32(gid.y) * block - phase;
    let b0 = max(s0, 0);
    if (line >= lines || b0 >= n) {
        return;
    }
    let norm = 1.0 / f32(2 * r + 1);
    var acc = vec4<f32>(0.0);
    for (var i = b0 - r; i <= b0 + r; i++) {
        acc += line_get(horizontal, line, i, n, repeat);
    }
    let b1 = min(s0 + block, n);
    for (var i = b0; i < b1; i++) {
        if (horizontal) {
            textureStore(out, vec2<i32>(i, line), acc * norm);
        } else {
            textureStore(out, vec2<i32>(line, i), acc * norm);
        }
        let add = line_get(horizontal, line, i + r + 1, n, repeat);
        let sub = line_get(horizontal, line, i - r, n, repeat);
        acc += add - sub;
    }
}

// The horizontal pass as `box_pass` runs it, one thread per row (the reference for `box_h`).
@compute @workgroup_size(64, 1)
fn box_h_serial(@builtin(global_invocation_id) gid: vec3<u32>) {
    box_pass(gid, true);
}

// `box_h`'s staging tiles: 8 columns of the workgroup's 64 rows (column-major), for the
// window's leading edge and its trailing edge (2r + 1 pixels behind).
var<workgroup> box_lead: array<vec4<f32>, 512>;
var<workgroup> box_trail: array<vec4<f32>, 512>;

// `box_pass` along rows with the reads staged through workgroup memory: the 64 threads load
// 8-pixel runs of their rows together (adjacent threads, adjacent pixels) instead of each
// reading its own row a pixel at a time. Each thread then sums exactly as `box_pass` does (the
// same values in the same order), so the output is bit-identical to `box_h_serial`.
// u[0] = (radius, repeat edges, block, phase as in `box_pass`); dispatched over (line, block).
@compute @workgroup_size(64, 1)
fn box_h(@builtin(global_invocation_id) gid: vec3<u32>, @builtin(local_invocation_index) t: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    let dims = out_dims();
    let r = i32(P.u[0].x);
    let repeat = P.u[0].y != 0u;
    let block = i32(P.u[0].z);
    let n = dims.x;
    let line = i32(gid.x);
    let s0 = i32(wg.y) * block - i32(P.u[0].w);
    let b0 = max(s0, 0);
    if (b0 >= n) {
        return;
    }
    let b1 = min(s0 + block, n);
    let norm = 1.0 / f32(2 * r + 1);
    // The leading edge runs over [b0 - r, b1 - 1 + r]: the first 2r + 1 pixels are the
    // starting sum, each later one moves the window by a pixel.
    let first = b0 - r;
    let last = b1 - 1 + r;
    let start = b0 + r;
    var acc = vec4<f32>(0.0);
    for (var c0 = first; c0 <= last; c0 += 8) {
        let trail = c0 + 7 > start;
        workgroupBarrier();
        for (var k = 0u; k < 8u; k++) {
            let i = k * 64u + t;
            let row = i / 8u;
            let col = i % 8u;
            let y = i32(wg.x * 64u + row);
            var a = vec4<f32>(0.0);
            var s = vec4<f32>(0.0);
            if (y < dims.y) {
                a = line_get(true, y, c0 + i32(col), n, repeat);
                if (trail) {
                    s = line_get(true, y, c0 + i32(col) - 2 * r - 1, n, repeat);
                }
            }
            box_lead[col * 64u + row] = a;
            box_trail[col * 64u + row] = s;
        }
        workgroupBarrier();
        for (var k = 0; k < 8; k++) {
            let p = c0 + k;
            if (p > last) {
                break;
            }
            let add = box_lead[u32(k) * 64u + t];
            if (p <= start) {
                acc += add;
            } else {
                let sub = box_trail[u32(k) * 64u + t];
                acc += add - sub;
            }
            if (p >= start && line < dims.y) {
                textureStore(out, vec2<i32>(p - r, line), acc * norm);
            }
        }
    }
}

@compute @workgroup_size(64, 1)
fn box_v(@builtin(global_invocation_id) gid: vec3<u32>) {
    box_pass(gid, false);
}

// Directional blur (raster::directional_blur). f[0] = (dx, dy, length, samples); u[0].x = repeat edges.
@compute @workgroup_size(16, 16)
fn directional(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let dx = P.f[0].x;
    let dy = P.f[0].y;
    let len = P.f[0].z;
    let n = i32(P.f[0].w);
    var acc = vec4<f32>(0.0);
    for (var i = 0; i < n; i++) {
        let t = (f32(i) / f32(n - 1) - 0.5) * len;
        let sx = f32(p.x) + 0.5 + dx * t;
        let sy = f32(p.y) + 0.5 + dy * t;
        if (P.u[0].x != 0u) {
            acc += sample_bilinear_clamped(src, sx, sy);
        } else {
            acc += sample_bilinear(src, sx, sy);
        }
    }
    textureStore(out, p, acc / f32(n));
}

// ---------------------------------------------------------------- Glow / Drop Shadow

// Glow's A & B gradient position (effects::misc::glow_ab_t). f[0].yzw = loops, phase, midpoint;
// u[0].z = colour looping.
fn glow_ab_t(l: f32) -> f32 {
    let u = clamp(l, 0.0, 1.0) * max(P.f[0].y, 0.0) + P.f[0].z;
    var t = u - floor(u);
    if (u > 0.0 && t <= 0.0) {
        t = 1.0;
    }
    let tri = 1.0 - abs(2.0 * t - 1.0);
    let lp = P.u[0].z;
    if (lp == 1u) {
        t = 1.0 - t;
    } else if (lp == 3u) {
        t = 1.0 - tri;
    } else if (lp != 0u) {
        t = tri;
    }
    let m = clamp(P.f[0].w, 0.01, 0.99);
    return pow(max(t, 0.0), log(0.5) / log(m));
}

// misc::glow_bright: the soft step at the threshold (f[0].x), misc::GLOW_KNEE wide, fading
// out towards black.
fn glow_step(v: f32) -> f32 {
    return clamp((v - P.f[0].x) / 0.12 + 0.5, 0.0, 1.0) * clamp(v / 0.12, 0.0, 1.0);
}

// Glow bright pass (misc::glow). u[0].x = one value per pixel (A & B Colors, Arbitrary Map),
// coloured after the blur; u[0].y = based on alpha; f[0].x = threshold.
@compute @workgroup_size(16, 16)
fn glow_bright(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let px = textureLoad(src, p, 0);
    let a = px.w;
    if (a <= 0.0) {
        textureStore(out, p, vec4<f32>(0.0));
        return;
    }
    if (P.u[0].x != 0u) {
        // The luminance, or the alpha channel.
        var l = luminance(px.xyz / a);
        if (P.u[0].y != 0u) {
            l = a;
        }
        textureStore(out, p, vec4<f32>(glow_step(l) * a));
    } else if (P.u[0].y != 0u) {
        textureStore(out, p, px * glow_step(a));
    } else {
        // Color Channels: each channel steps on its own.
        let c = vec3<f32>(glow_step(px.x / a), glow_step(px.y / a), glow_step(px.z / a));
        textureStore(out, p, vec4<f32>(c * a, max(max(c.x, c.y), c.z) * a));
    }
}

// A & B Colors after the blur (misc::glow): the gradient at misc::glow_ab_position of the
// blurred strength (alpha), premultiplied by it. Same f / u layout as `glow_bright`;
// f[1] = colour A; f[2] = colour B.
@compute @workgroup_size(16, 16)
fn glow_ab(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let v = clamp(textureLoad(src, p, 0).w, 0.0, 1.0);
    let t = 1.0 - glow_ab_t(v);
    let c = P.f[1].xyz + (P.f[2].xyz - P.f[1].xyz) * t;
    textureStore(out, p, vec4<f32>(c * v, v));
}

// Glow composite: original `src` with the blurred glow `aux`. u[0].x = misc::glow_composite:
// 0 the glow over the original (Behind / None), 1 the glow behind it (On Top);
// f[0].x = intensity.
@compute @workgroup_size(16, 16)
fn glow_combine(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    var o = textureLoad(src, p, 0);
    let g = textureLoad(aux, p, 0) * P.f[0].x;
    if (P.u[0].x == 1u) {
        let k = 1.0 - o.w;
        o = o + g * k;
    } else {
        o = vec4<f32>(o.xyz + g.xyz, min(o.w + g.w * (1.0 - o.w), 1.0));
    }
    o.w = clamp(o.w, 0.0, 1.0);
    textureStore(out, p, o);
}

// Drop shadow silhouette. f[0] = shadow colour; f[1] = (opacity, dx, dy).
@compute @workgroup_size(16, 16)
fn shadow_make(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let c = P.f[0];
    let a = sample_bilinear(src, f32(p.x) + 0.5 - P.f[1].y, f32(p.y) + 0.5 - P.f[1].z).w * P.f[1].x * c.w;
    textureStore(out, p, vec4<f32>(c.xyz * a, a));
}

// The layer over its shadow: `aux` (original) over `src` (shadow).
@compute @workgroup_size(16, 16)
fn shadow_combine(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let s = textureLoad(src, p, 0);
    let o = textureLoad(aux, p, 0);
    textureStore(out, p, o + s * (1.0 - o.w));
}

// ---------------------------------------------------------------- per-pixel effects

fn mix3(a: vec3<f32>, b: vec3<f32>, t: f32) -> vec3<f32> {
    return a + (b - a) * t;
}

// Curves: data = [n, xs.., ys.., ms.., lut (1025)] per channel at offsets f[c].
fn curve_exact(base: u32, x: f32) -> f32 {
    let n = u32(data[base]);
    let xs = base + 1u;
    let ys = xs + n;
    let ms = ys + n;
    if (x <= data[xs]) {
        return data[ys] + (x - data[xs]) * data[ms];
    }
    if (x >= data[xs + n - 1u]) {
        return data[ys + n - 1u] + (x - data[xs + n - 1u]) * data[ms + n - 1u];
    }
    // partition_point(|v| v <= x) - 1, clamped to n - 2.
    var k = 0u;
    for (var i = 0u; i < n; i++) {
        if (data[xs + i] <= x) {
            k = i + 1u;
        }
    }
    k = min(max(k, 1u) - 1u, n - 2u);
    let h = data[xs + k + 1u] - data[xs + k];
    let t = (x - data[xs + k]) / h;
    let t2 = t * t;
    let t3 = t * t * t;
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;
    return h00 * data[ys + k] + h10 * h * data[ms + k] + h01 * data[ys + k + 1u] + h11 * h * data[ms + k + 1u];
}

fn curve_eval(base_f: f32, x: f32) -> f32 {
    if (base_f < 0.0) {
        return x;
    }
    let base = u32(base_f);
    if (!(x >= 0.0 && x <= 1.0)) {
        return curve_exact(base, x);
    }
    let n = u32(data[base]);
    let lut = base + 1u + 3u * n;
    let f = x * 1024.0;
    let i = min(u32(f), 1023u);
    let t = f - f32(i);
    return data[lut + i] + (data[lut + i + 1u] - data[lut + i]) * t;
}

// Fractal Noise lattice (effects::generate::lattice / value_noise).
fn lattice(ix: i32, iy: i32, iz: i32, seed: u32) -> f32 {
    var h = (bitcast<u32>(ix) * 0x27d4eb2du) ^ (bitcast<u32>(iy) * 0x165667b1u) ^ (bitcast<u32>(iz) * 0x9e3779b9u) ^ (seed * 0x85ebca6bu);
    h = h ^ (h >> 15u);
    h = h * 0x2c1b3c6du;
    h = h ^ (h >> 12u);
    h = h * 0x297a2d39u;
    h = h ^ (h >> 15u);
    return f32(h & 0xffffu) / 65535.0;
}

fn fade(t: f32) -> f32 {
    return t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
}

fn value_noise(x: f32, y: f32, z: f32, seed: u32) -> f32 {
    let x0 = floor(x);
    let y0 = floor(y);
    let z0 = floor(z);
    let fx = fade(x - x0);
    let fy = fade(y - y0);
    let fz = fade(z - z0);
    let ix = i32(x0);
    let iy = i32(y0);
    let iz = i32(z0);
    let c000 = lattice(ix, iy, iz, seed);
    let c100 = lattice(ix + 1, iy, iz, seed);
    let c010 = lattice(ix, iy + 1, iz, seed);
    let c110 = lattice(ix + 1, iy + 1, iz, seed);
    let c001 = lattice(ix, iy, iz + 1, seed);
    let c101 = lattice(ix + 1, iy, iz + 1, seed);
    let c011 = lattice(ix, iy + 1, iz + 1, seed);
    let c111 = lattice(ix + 1, iy + 1, iz + 1, seed);
    let l00 = c000 + (c100 - c000) * fx;
    let l01 = c010 + (c110 - c010) * fx;
    let a = l00 + (l01 - l00) * fy;
    let l10 = c001 + (c101 - c001) * fx;
    let l11 = c011 + (c111 - c011) * fx;
    let b = l10 + (l11 - l10) * fy;
    return a + (b - a) * fz;
}

// Fractal Noise's Soft Linear (effects::noise3::typed_noise): the quadratic B-spline over the
// 3 × 3 lattice values around the nearest one, faded along evolution, and SOFT_LINEAR_GAIN.
const SOFT_LINEAR_GAIN: f32 = (181.0 / 231.0) / 0.55;

fn bspline(t: f32) -> vec3<f32> {
    let s = 1.0 - t;
    return vec3<f32>(0.5 * s * s, 0.5 + t * s, 0.5 * t * t);
}

fn soft_linear_slice(ix: i32, iy: i32, iz: i32, wx: vec3<f32>, wy: vec3<f32>, seed: u32) -> f32 {
    var sum = 0.0;
    for (var j = 0; j < 3; j++) {
        let y = iy + j - 1;
        sum += wy[j] * (wx.x * lattice(ix - 1, y, iz, seed) + wx.y * lattice(ix, y, iz, seed) + wx.z * lattice(ix + 1, y, iz, seed));
    }
    return sum;
}

fn soft_linear_noise(x: f32, y: f32, z: f32, seed: u32) -> f32 {
    let x0 = floor(x + 0.5);
    let y0 = floor(y + 0.5);
    let z0 = floor(z);
    let wx = bspline(x + 0.5 - x0);
    let wy = bspline(y + 0.5 - y0);
    let a = soft_linear_slice(i32(x0), i32(y0), i32(z0), wx, wy, seed);
    let b = soft_linear_slice(i32(x0), i32(y0), i32(z0) + 1, wx, wy, seed);
    let n = a + (b - a) * fade(z - z0);
    return 0.5 + (n - 0.5) * SOFT_LINEAR_GAIN;
}

// generate::put: blend a generated straight colour (alpha ga) over the pixel.
fn put(px: vec4<f32>, g: vec3<f32>, ga: f32, blend_orig: f32) -> vec4<f32> {
    let g2 = vec4<f32>(g * ga, ga);
    let k = 1.0 - blend_orig;
    return px * blend_orig + g2 * k;
}

// Straight-colour map helper: skips transparent pixels (Image::map_straight).
fn straight(px: vec4<f32>) -> vec3<f32> {
    return px.xyz / px.w;
}

// u[0].x = op:
//  1 Tint            f[0] = black, f[1] = white, f[2].x = amount
//  2 Bright/Contrast f[0] = (brightness, k)
//  3 Hue/Saturation  u[0].y = colorize; f[0] = (hue, sat, light); f[1] = (ch, cs, cl)
//  4 Levels          f[0] = (ib, iw, g, ob); f[1] = (ow); u[0].y = clip
//  5 Exposure        f[0] = (e, offset, gamma)
//  6 Invert          u[0].y = channel; f[0].x = blend
//  7 Fill            f[0] = colour; f[1] = (opacity); u[0].y = invert
//  8 Curves          f[0] = data offsets (rgb, red, green, blue), f[1].x = alpha (−1 = none)
//  9 Gradient Ramp   f[0] = (sx, sy, ex, ey); f[1] = start colour; f[2] = end colour;
//                    f[3] = (len2, scatter, blend); u[0] = (_, radial, seed);
//                    f[4], f[5] = rows of the pixel → ramp-space affine (comp space)
// 10 Fractal Noise   f[0] = (contrast, brightness, scale x, frac); f[1] = (ox, oy, sr, cr);
//                    f[2] = (evo, blend, opacity, scale y); f[3] = (sub influence, 1 / sub scaling);
//                    u[0] = (_, kind, n_oct, seed); u[1] = (invert, overflow, normal blend)
@compute @workgroup_size(16, 16)
fn pointwise(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, pointwise_px(textureLoad(src, p, 0), P, p, gid));
}

fn colour_tint(input: vec4<f32>, black: vec3<f32>, white: vec3<f32>, amount: f32) -> vec4<f32> {
    var px = input;
    let a = px.w;
    if (a > 0.0) {
        let c = straight(px);
        let l = clamp(luminance(c), 0.0, 1.0);
        let blk = black;
        let wht = white;
        let t = blk + (wht - blk) * l;
        px = vec4<f32>(mix3(c, t, amount) * a, a);
    }
    return px;
}

fn colour_levels(input: vec4<f32>, flags: vec2<u32>, input_range: vec4<f32>, output_white: f32) -> vec4<f32> {
    var px = input;
    let a = px.w;
    if (a > 0.0) {
        let c = straight(px);
        let ib = input_range.x;
        let iw = input_range.y;
        let g = input_range.z;
        let ob = input_range.w;
        let ow = output_white;
        var o = vec3<f32>(0.0);
        for (var i = 0; i < 3; i++) {
            var t = (c[i] - ib) / max(iw - ib, 1e-6);
            if (flags.x != 0u) {
                t = max(t, 0.0);
            }
            if (flags.y != 0u) {
                t = min(t, 1.0);
            }
            t = powz(max(t, 0.0), 1.0 / g);
            o[i] = ob + (ow - ob) * t;
        }
        px = vec4<f32>(o * a, a);
    }
    return px;
}

fn colour_exposure(input: vec4<f32>, linear: u32, gain: vec3<f32>, offset: vec3<f32>, gamma: vec3<f32>) -> vec4<f32> {
    var px = input;
    let a = px.w;
    if (a > 0.0) {
        let c = straight(px);
        var o = vec3<f32>(0.0);
        for (var i = 0; i < 3; i++) {
            // Per-channel gain, offset, gamma; negative values are mirrored.
            if (linear != 0u) {
                let x = c[i] * gain[i] + offset[i];
                o[i] = sign(x) * powz(abs(x), 1.0 / gamma[i]);
            } else {
                let lin = srgb_to_linear(max(c[i], 0.0));
                let x = lin * gain[i] + offset[i];
                o[i] = sign(x) * linear_to_srgb(powz(abs(x), 1.0 / gamma[i]));
            }
        }
        px = vec4<f32>(o * a, a);
    }
    return px;
}

fn colour_curves(input: vec4<f32>, offsets: vec4<f32>, alpha_offset: f32) -> vec4<f32> {
    var px = input;
    let a = px.w;
    // effects::color2::map_ca: transparent pixels only change through the alpha curve.
    if (a <= 0.0 && px.x == 0.0 && px.y == 0.0 && px.z == 0.0) {
        let na = curve_eval(alpha_offset, 0.0);
        if (na > 0.0) {
            px = vec4<f32>(0.0, 0.0, 0.0, clamp(na, 0.0, 1.0));
        }
    } else {
        var c = vec3<f32>(0.0);
        var aa = max(a, 0.0);
        if (a > 1e-6) {
            c = px.xyz / a;
            aa = a;
        }
        var o = vec3<f32>(0.0);
        for (var i = 0; i < 3; i++) {
            o[i] = curve_eval(offsets[i + 1], curve_eval(offsets.x, c[i]));
        }
        let na = clamp(curve_eval(alpha_offset, aa), 0.0, 1.0);
        px = vec4<f32>(o * na, na);
    }
    return px;
}

fn pointwise_px(input: vec4<f32>, params: Params, p: vec2<i32>, gid: vec3<u32>) -> vec4<f32> {
    var px = input;
    let a = px.w;
    let op = params.u[0].x;
    switch op {
        case 1u: { px = colour_tint(px, params.f[0].xyz, params.f[1].xyz, params.f[2].x); }
        case 2u: {
            if (a > 0.0) {
                let c = straight(px);
                let br = params.f[0].x;
                let k = params.f[0].y;
                let ct = params.f[0].z;
                if (params.u[0].y == 1u) {
                    px = vec4<f32>(max((c + br - 0.5) * k + 0.5, vec3<f32>(0.0)) * a, a);
                } else if (params.u[0].y == 2u) {
                    // Brightness & Contrast tone curves (effects crate bc_modern, AE-fitted #681).
                    var o = c;
                    let g = max(1.0 + 0.53 * ct, 0.05);
                    for (var i = 0; i < 3; i++) {
                        if (c[i] < 1.0) {
                            var v = max(c[i], 0.0);
                            if (br > 0.0) {
                                v = 1.0 - powz(1.0 - v, exp2(1.24 * br));
                            } else if (br < 0.0) {
                                v = powz(v, exp2(-1.32 * br));
                            }
                            if (ct != 0.0) {
                                if (v < 0.5) {
                                    v = 0.5 * powz(2.0 * v, g);
                                } else {
                                    v = 1.0 - 0.5 * powz(max(2.0 - 2.0 * v, 0.0), g);
                                }
                            }
                            o[i] = v;
                        }
                    }
                    px = vec4<f32>(o * a, a);
                }
            }
        }
        case 3u: {
            if (a > 0.0) {
                let c = straight(px);
                let hsl = rgb_to_hsl(c);
                var h = hsl.x;
                var s = hsl.y;
                var l = hsl.z;
                if (params.u[0].y != 0u) {
                    let cl = params.f[1].z;
                    var k = l;
                    if (cl > 0.0) {
                        k = 1.0 - l;
                    }
                    h = params.f[1].x;
                    s = params.f[1].y;
                    l = clamp(l + cl * k, 0.0, 1.0);
                } else {
                    let hue = params.f[0].x;
                    let sat = params.f[0].y;
                    let light = params.f[0].z;
                    var s2: f32;
                    if (sat >= 0.0) {
                        s2 = s + (1.0 - s) * sat * min(s, 1.0);
                    } else {
                        s2 = s * (1.0 + sat);
                    }
                    var l2: f32;
                    if (light >= 0.0) {
                        l2 = l + (1.0 - l) * light;
                    } else {
                        l2 = l * (1.0 + light);
                    }
                    h = fract_euclid(h + hue);
                    s = clamp(s2, 0.0, 1.0);
                    l = l2;
                }
                px = vec4<f32>(hsl_to_rgb(h, s, l) * a, a);
            }
        }
        case 4u: { px = colour_levels(px, params.u[0].yz, params.f[0], params.f[1].x); }
        case 5u: { px = colour_exposure(px, params.u[0].y, params.f[0].xyz, params.f[1].xyz, params.f[2].xyz); }
        case 6u: {
            let ch = params.u[0].y;
            let blend = params.f[0].x;
            if (ch == 4u) {
                let na = 1.0 - a;
                var k = 0.0;
                if (a > 0.0) {
                    k = na / a;
                }
                let inv = vec4<f32>(px.xyz * k, na);
                px = px + (inv - px) * blend;
            } else if (a > 0.0) {
                let s = straight(px);
                var o = s;
                switch ch {
                    case 1u: { o.x = 1.0 - s.x; }
                    case 2u: { o.y = 1.0 - s.y; }
                    case 3u: { o.z = 1.0 - s.z; }
                    default: { o = 1.0 - s; }
                }
                px = vec4<f32>((s + (o - s) * blend) * a, a);
            }
        }
        case 7u: {
            let c = params.f[0];
            let invert = params.u[0].y != 0u;
            let opacity = params.f[1].x;
            var fa = px.w;
            if (invert) {
                fa = 1.0 - px.w;
            }
            let f = c.xyz * fa;
            let rgb = px.xyz + (f - px.xyz) * opacity;
            var alpha = px.w;
            if (invert) {
                alpha = px.w + (fa - px.w) * opacity;
            }
            px = vec4<f32>(rgb, alpha);
        }
        case 8u: { px = colour_curves(px, params.f[0], params.f[1].x); }
        case 9u: {
            let s = params.f[0].xy;
            let e = params.f[0].zw;
            let d = e - s;
            let pc = vec3<f32>(f32(p.x) + 0.5, f32(p.y) + 0.5, 1.0);
            let v = vec2<f32>(dot(params.f[4].xyz, pc), dot(params.f[5].xyz, pc)) - s;
            let len2 = params.f[3].x;
            var t: f32;
            if (params.u[0].y != 0u) {
                t = sqrt((v.x * v.x + v.y * v.y) / len2);
            } else {
                t = (v.x * d.x + v.y * d.y) / len2;
            }
            let scatter = params.f[3].y;
            if (scatter > 0.0) {
                t += (hash_noise(gid.x, gid.y, params.u[0].z) - 0.5) * scatter;
            }
            t = clamp(t, 0.0, 1.0);
            let c0 = params.f[1].xyz;
            let c1 = params.f[2].xyz;
            px = put(px, c0 + (c1 - c0) * t, px.w, params.f[3].z);
        }
        case 10u: {
            let contrast = params.f[0].x;
            let brightness = params.f[0].y;
            let scale = params.f[0].z;
            let scale_y = params.f[2].w;
            let infl = params.f[3].x;
            let fmul = params.f[3].y;
            let frac = params.f[0].w;
            let ox = params.f[1].x;
            let oy = params.f[1].y;
            let sr = params.f[1].z;
            let cr = params.f[1].w;
            let evo = params.f[2].x;
            let kind = params.u[0].y;
            let n_oct = params.u[0].z;
            let seed = params.u[0].w;
            let dx = f32(p.x) + 0.5 - ox;
            let dy = f32(p.y) + 0.5 - oy;
            let u = (dx * cr + dy * sr) / scale;
            let v = (-dx * sr + dy * cr) / scale_y;
            var sum = 0.0;
            var amp = 1.0;
            var norm = 0.0;
            var f = 1.0;
            for (var o = 0u; o < n_oct; o++) {
                var w = 1.0;
                if (o + 1u == n_oct && frac > 0.0) {
                    w = frac;
                }
                var n = soft_linear_noise(u * f, v * f, evo + f32(o) * 7.31, seed + o);
                let s = n * 2.0 - 1.0;
                if (kind == 1u) {
                    n = 1.0 - s * s;
                } else if (kind == 2u) {
                    n = 1.0 - abs(s);
                }
                sum += n * amp * w;
                norm += amp * w;
                amp *= infl;
                f *= fmul;
            }
            var val = sum / max(norm, 1e-6);
            val = (val - 0.5) * contrast + 0.5 + brightness;
            // Overflow: Clip, Soft Clamp, Wrap Back, Allow HDR Results.
            let ov = params.u[1].y;
            if (ov == 0u) {
                val = clamp(val, 0.0, 1.0);
            } else if (ov == 1u) {
                val = clamp(0.5 + 0.5 * tanh(2.0 * (val - 0.5)) / tanh(1.0), 0.0, 1.0);
            } else if (ov == 2u) {
                let t = val - 2.0 * floor(val / 2.0);
                val = select(t, 2.0 - t, t > 1.0);
            }
            if (params.u[1].x != 0u) {
                val = 1.0 - val;
            }
            let op = params.f[2].z;
            var fx = vec4<f32>(vec3<f32>(val) * op, op);
            if (params.u[1].z == 1u) {
                // Normal: the noise over the layer at Opacity.
                fx = fx + px * (1.0 - op);
            }
            // Inside the layer's alpha (its masks), then Blend With Original.
            px = px * params.f[2].y + fx * px.w * (1.0 - params.f[2].y);
        }
        default: {}
    }
    return px;
}

// Adjustment layer finish (Renderer::draw_adjustment): canvas `src` moves toward the adjusted
// comp `aux` by the footprint's alpha × opacity. The footprint is `data` (RGBA f32 rows of
// u[0].x pixels); f[0].x = opacity.
@compute @workgroup_size(16, 16)
fn adjust_mix(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    let c = textureLoad(src, p, 0);
    let a = textureLoad(aux, p, 0);
    let k = data[(gid.y * P.u[0].x + gid.x) * 4u + 3u] * P.f[0].x;
    textureStore(out, p, c + (a - c) * k);
}

// ---------------------------------------------------------------- Classic 3D bokeh depth of field
// (render::three_d::bokeh: boost_highlights, row prefix sums, span gathers per blur level
// blended by each pixel's radius.)

// Highlight boost: f[0] = (gain, threshold, saturation).
@compute @workgroup_size(16, 16)
fn bokeh_boost(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    var c = textureLoad(src, p, 0);
    let gain = P.f[0].x;
    let thr = clamp(P.f[0].y, 0.0, 1.0);
    let satk = P.f[0].z;
    if (c.w > 0.0 && gain > 0.0) {
        let a = c.w;
        let rgb = c.xyz / a;
        let l = 0.2126 * rgb.x + 0.7152 * rgb.y + 0.0722 * rgb.z;
        if (!(l < thr || (thr >= 1.0 && l < 1.0))) {
            var over = 1.0;
            if (thr < 1.0) {
                over = clamp((l - thr) / (1.0 - thr), 0.0, 1.0);
            }
            let k = 1.0 + gain * 4.0 * max(over, 0.25);
            let sat = 1.0 + satk;
            let o = max(vec3<f32>(l) + (rgb - vec3<f32>(l)) * sat, vec3<f32>(0.0)) * k;
            c = vec4<f32>(o * a, a);
        }
    }
    textureStore(out, p, c);
}

// Row prefix sums: out is (w + 1) × h, out[x] = sum of src[0..x]. One invocation per row.
@compute @workgroup_size(64, 1)
fn bokeh_prefix(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(src));
    let y = i32(gid.x);
    if (y >= dims.y) {
        return;
    }
    var acc = vec4<f32>(0.0);
    textureStore(out, vec2<i32>(0, y), acc);
    for (var x = 0; x < dims.x; x = x + 1) {
        acc = acc + textureLoad(src, vec2<i32>(x, y), 0);
        textureStore(out, vec2<i32>(x + 1, y), acc);
    }
}

fn bokeh_prefix_at(y: i32, t: f32) -> vec4<f32> {
    let n = i32(textureDimensions(src).x) - 1;
    if (t <= 0.0) {
        return vec4<f32>(0.0);
    }
    if (t >= f32(n)) {
        return textureLoad(src, vec2<i32>(n, y), 0);
    }
    let i = i32(floor(t));
    let f = t - f32(i);
    let a = textureLoad(src, vec2<i32>(i, y), 0);
    let b = textureLoad(src, vec2<i32>(i + 1, y), 0);
    return a + (b - a) * f;
}

// One blur level added into the accumulator `aux`: src = prefix sums; data = the radius map
// (w·h) then spans (dy, x0, x1, weight). u[0] = (span offset, span count, level, levels),
// u[1].x = 1 for the first level (no accumulator yet); f[0] = (1 / norm, lo, hi).
@compute @workgroup_size(16, 16)
fn bokeh_gather(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    var acc = vec4<f32>(0.0);
    if (P.u[1].x == 0u) {
        acc = textureLoad(aux, p, 0);
    }
    let r = data[u32(p.y) * u32(dims.x) + u32(p.x)];
    let k = P.u[0].z;
    let n = P.u[0].w;
    var wgt = 1.0;
    if (n > 1u) {
        let lo = P.f[0].y;
        let hi = P.f[0].z;
        let f = clamp((r - lo) / (hi - lo) * f32(n - 1u), 0.0, f32(n - 1u));
        let kk = min(u32(floor(f)), n - 2u);
        let t = f - f32(kk);
        if (k == kk) {
            wgt = 1.0 - t;
        } else if (k == kk + 1u) {
            wgt = t;
        } else {
            wgt = 0.0;
        }
    }
    if (wgt > 0.0) {
        var g = vec4<f32>(0.0);
        let off = P.u[0].x;
        for (var i = 0u; i < P.u[0].y; i = i + 1u) {
            let s = off + i * 4u;
            let sy = p.y + i32(data[s]);
            if (sy < 0 || sy >= dims.y) {
                continue;
            }
            let hi = bokeh_prefix_at(sy, f32(p.x) + data[s + 2u] + 0.5);
            let lo = bokeh_prefix_at(sy, f32(p.x) + data[s + 1u] + 0.5);
            g = g + (hi - lo) * data[s + 3u];
        }
        acc = acc + g * P.f[0].x * wgt;
    }
    textureStore(out, p, acc);
}

// ---------------------------------------------------------------- pool

// Set every pixel to f[0] (clearing reused working textures; NaN poison under test).
@compute @workgroup_size(16, 16)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, P.f[0]);
}
