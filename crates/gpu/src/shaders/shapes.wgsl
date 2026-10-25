// Shape layer rasteriser (`shapes_gpu.rs`): the CPU plans (`effectcraft_render::shapes::gpu_plan`)
// and builds each fill's scan edges exactly as tiny-skia does (`shapes::skia_scan`); these
// kernels walk them. Per draw: `sh_accum` (one invocation per edge chunk) adds each edge's
// winding at its sample column on every sub-scanline it spans (4 sub-scanlines per pixel row, 4
// sample columns per pixel, packed as four 8-bit lanes per pixel and sub-scanline); `sh_row`
// (one workgroup per pixel row) prefix-sums the windings along the row, counts the covered
// samples into tiny-skia's coverage (16 per sample, 64 / 63 for a whole sub-scanline, capped at
// 255), paints the draw's colour or gradient with it onto the layer image (Normal, as
// `shapes::paint`) and zeroes the accumulation it read for the next draw. `sh_store` writes the
// layer image into the compositor's texture.

struct Draw {
    // x0, y0, w, h of the draw's mask rectangle in buffer pixels
    r: vec4<u32>,
    // unused, edge count, first edge, flags (1: even-odd, 2: gradient)
    m: vec4<u32>,
    // image width, image height, first gradient word, gradient kind
    n: vec4<u32>,
    // straight RGBA (solid draws)
    color: vec4<f32>,
    // opacity, len2, has affine, has focal
    op: vec4<f32>,
    // buffer pixel -> paint space: rows (a, c, e) and (b, d, f)
    lx: vec4<f32>,
    ly: vec4<f32>,
    // paint -> unit gradient space (affine gradients), same layout
    ax: vec4<f32>,
    ay: vec4<f32>,
    // ramp start (xy) and direction (zw)
    sd: vec4<f32>,
    // focal point (xy)
    fo: vec4<f32>,
}

@group(0) @binding(0) var<uniform> D: Draw;
// [first sub-scanline, (count << 1) | (winding < 0), x (16.16), dx (16.16)]
@group(0) @binding(1) var<storage, read> edges: array<vec4<i32>>;
// Per mask pixel and sub-scanline: winding deltas of its 4 sample columns, 8-bit lanes.
@group(0) @binding(2) var<storage, read_write> acc: array<atomic<i32>>;
@group(0) @binding(3) var<storage, read_write> img: array<vec4<f32>>;
@group(0) @binding(4) var<storage, read> grad: array<f32>;
@group(0) @binding(5) var out: texture_storage_2d<rgba32float, write>;

@compute @workgroup_size(64, 1, 1)
fn sh_accum(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= D.m.y) {
        return;
    }
    let e = edges[D.m.z + i];
    let count = e.y >> 1u;
    var wind = 1;
    if ((e.y & 1) != 0) {
        wind = -1;
    }
    let w = i32(D.r.z);
    let rows = i32(D.r.w) * 4;
    for (var k = 0; k < count; k++) {
        let sy = e.x + k;
        if (sy < 0 || sy >= rows) {
            continue;
        }
        // The walk's fixed-point x on this sub-scanline, rounded to a sample column.
        let x = e.z + k * e.w;
        let sx = max((x + 32768) >> 16u, 0);
        if (sx >= w * 4) {
            continue;
        }
        let at = ((u32(sy >> 2u) * D.r.z + u32(sx >> 2u)) << 2u) + u32(sy & 3);
        atomicAdd(&acc[at], wind << (8u * u32(sx & 3)));
    }
}

// The four signed lane sums of a packed sum of lane deltas (`d0 + 256 d1 + 65536 d2 + 2^24 d3`
// modulo 2^32, each |dk| < 128): low lane first, each borrow taken out before the next.
fn lanes(v: i32) -> vec4<i32> {
    let d0 = (v << 24u) >> 24u;
    var r = (v - d0) >> 8u;
    let d1 = (r << 24u) >> 24u;
    r = (r - d1) >> 8u;
    let d2 = (r << 24u) >> 24u;
    r = (r - d2) >> 8u;
    let d3 = (r << 24u) >> 24u;
    return vec4<i32>(d0, d1, d2, d3);
}

// ---------------------------------------------------------------- gradients (shapes::GradientRamp)

fn g_at(i: u32) -> f32 {
    return grad[D.n.z + i];
}

fn ramp_t(p_in: vec2<f32>) -> f32 {
    var p = p_in;
    if (D.op.z > 0.5) {
        p = vec2<f32>(D.ax.x * p.x + D.ax.y * p.y + D.ax.z, D.ay.x * p.x + D.ay.y * p.y + D.ay.z);
    }
    let s = D.sd.xy;
    let d = D.sd.zw;
    let len2 = D.op.y;
    let v = p - s;
    let kind = D.n.w;
    if (kind == 2u) {
        let a = (atan2(v.y, v.x) - atan2(d.y, d.x)) / 6.283185307179586;
        return a - floor(a);
    }
    if (kind == 3u) {
        return (abs(v.x * d.x + v.y * d.y) + abs(v.y * d.x - v.x * d.y)) / len2;
    }
    if (kind == 0u) {
        return (v.x * d.x + v.y * d.y) / len2;
    }
    if (D.op.w < 0.5) {
        return sqrt((v.x * v.x + v.y * v.y) / len2);
    }
    let f = D.fo.xy;
    let q = p - f;
    let dist = length(q);
    if (dist < 1e-12) {
        return 0.0;
    }
    let u = q / dist;
    let c = f - s;
    let b = dot(u, c);
    let cc = dot(c, c) - len2;
    let lambda = -b + sqrt(max(b * b - cc, 0.0));
    if (lambda <= 1e-12) {
        return 1.0;
    }
    return dist / lambda;
}

// sample_stops' midpoint remap.
fn mid_f(f: f32, m: f32) -> f32 {
    if (f <= m) {
        return 0.5 * f / m;
    }
    return 0.5 + 0.5 * (f - m) / (1.0 - m);
}

// Gradient::sample: colour stops (pos, r, g, b, a, mid) then opacity stops (pos, a, mid).
fn gradient_at(t_in: f32) -> vec4<f32> {
    let t = clamp(t_in, 0.0, 1.0);
    let nc = u32(g_at(0u));
    let no = u32(g_at(1u));
    var col = vec4<f32>(1.0);
    if (nc > 0u) {
        let c0 = 2u;
        col = vec4<f32>(g_at(c0 + 1u), g_at(c0 + 2u), g_at(c0 + 3u), g_at(c0 + 4u));
        if (t > g_at(c0)) {
            var found = false;
            for (var i = 0u; i + 1u < nc; i++) {
                let a = c0 + i * 6u;
                let b = a + 6u;
                if (t <= g_at(b)) {
                    let span = max(g_at(b) - g_at(a), 1e-12);
                    let f = mid_f(clamp((t - g_at(a)) / span, 0.0, 1.0), g_at(a + 5u));
                    let ca = vec4<f32>(g_at(a + 1u), g_at(a + 2u), g_at(a + 3u), g_at(a + 4u));
                    let cb = vec4<f32>(g_at(b + 1u), g_at(b + 2u), g_at(b + 3u), g_at(b + 4u));
                    col = ca + (cb - ca) * f;
                    found = true;
                    break;
                }
            }
            if (!found) {
                let l = c0 + (nc - 1u) * 6u;
                col = vec4<f32>(g_at(l + 1u), g_at(l + 2u), g_at(l + 3u), g_at(l + 4u));
            }
        }
    }
    var alpha = 1.0;
    if (no > 0u) {
        let o0 = 2u + nc * 6u;
        alpha = g_at(o0 + 1u);
        if (t > g_at(o0)) {
            var found = false;
            for (var i = 0u; i + 1u < no; i++) {
                let a = o0 + i * 3u;
                let b = a + 3u;
                if (t <= g_at(b)) {
                    let span = max(g_at(b) - g_at(a), 1e-12);
                    let f = mid_f(clamp((t - g_at(a)) / span, 0.0, 1.0), g_at(a + 2u));
                    alpha = g_at(a + 1u) + (g_at(b + 1u) - g_at(a + 1u)) * f;
                    found = true;
                    break;
                }
            }
            if (!found) {
                alpha = g_at(o0 + (no - 1u) * 3u + 1u);
            }
        }
    }
    return vec4<f32>(col.rgb, col.a * alpha);
}

fn paint_color(bx: u32, by: u32) -> vec4<f32> {
    if ((D.m.w & 2u) == 0u) {
        return D.color;
    }
    let p = vec2<f32>(f32(bx) + 0.5, f32(by) + 0.5);
    let q = vec2<f32>(D.lx.x * p.x + D.lx.y * p.y + D.lx.z, D.ly.x * p.x + D.ly.y * p.y + D.ly.z);
    return gradient_at(ramp_t(q));
}

// ---------------------------------------------------------------- rows

var<workgroup> sums: array<vec4<i32>, 256>;

@compute @workgroup_size(256, 1, 1)
fn sh_row(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let y = wid.y;
    if (y >= D.r.w) {
        return;
    }
    let w = D.r.z;
    let tiles = (w + 255u) / 256u;
    let even_odd = (D.m.w & 1u) != 0u;
    var carry = vec4<i32>(0);
    for (var t = 0u; t < tiles; t++) {
        let x = t * 256u + lid.x;
        // Per sub-scanline j: the winding deltas at this pixel's 4 sample columns.
        var dl: array<vec4<i32>, 4>;
        var total = vec4<i32>(0);
        if (x < w) {
            let at = (y * w + x) << 2u;
            for (var j = 0u; j < 4u; j++) {
                let d = lanes(atomicExchange(&acc[at + j], 0));
                dl[j] = d;
                total[j] = d.x + d.y + d.z + d.w;
            }
        }
        sums[lid.x] = total;
        workgroupBarrier();
        for (var off = 1u; off < 256u; off = off << 1u) {
            var add = vec4<i32>(0);
            if (lid.x >= off) {
                add = sums[lid.x - off];
            }
            workgroupBarrier();
            sums[lid.x] = sums[lid.x] + add;
            workgroupBarrier();
        }
        // Winding left of this pixel, per sub-scanline.
        let before = carry + sums[lid.x] - total;
        carry = carry + sums[255];
        workgroupBarrier();
        if (x < w) {
            var alpha = 0u;
            for (var j = 0u; j < 4u; j++) {
                var wv = before[j];
                var n = 0u;
                for (var k = 0u; k < 4u; k++) {
                    wv += dl[j][k];
                    if ((even_odd && (wv & 1) != 0) || (!even_odd && wv != 0)) {
                        n += 1u;
                    }
                }
                if (n == 4u) {
                    alpha += select(64u, 63u, j == 3u);
                } else {
                    alpha += 16u * n;
                }
            }
            paint_px(D.r.x + x, D.r.y + y, f32(min(alpha, 255u)) / 255.0);
        }
    }
}

// Paint pixel (bx, by) of the layer image with coverage `c` (`shapes::paint`, Normal).
fn paint_px(bx: u32, by: u32, c: f32) {
    if (c <= 0.0 || bx >= D.n.x || by >= D.n.y) {
        return;
    }
    let col = paint_color(bx, by);
    let al = c * D.op.x * col.a;
    if (al <= 0.0) {
        return;
    }
    let i = by * D.n.x + bx;
    let px = img[i];
    img[i] = vec4<f32>(col.rgb * al, al) + px * (1.0 - al);
}

@compute @workgroup_size(16, 16, 1)
fn sh_store(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= D.n.x || gid.y >= D.n.y) {
        return;
    }
    textureStore(out, vec2<i32>(gid.xy), img[gid.y * D.n.x + gid.x]);
}
