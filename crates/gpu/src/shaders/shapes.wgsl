// Shape and text layer rasteriser (`shapes_gpu.rs`): the CPU plans (`effectcraft_render::shapes::
// gpu_plan`, `effectcraft_render::text::gpu_plan`) and builds each fill's scan edges exactly as
// tiny-skia does (`shapes::skia_scan`); these kernels walk them, every draw of a batch at once:
// `sh_accum` (one invocation per edge chunk) adds each edge's winding at its sample column on
// every sub-scanline it spans, in its draw's own region of `acc` (4 sub-scanlines per pixel row,
// 4 sample columns per pixel, packed as four 8-bit lanes per pixel and sub-scanline); `sh_cov`
// (one workgroup per row of a draw, 64 pixels a step) prefix-sums the windings along the row, counts the covered
// samples into tiny-skia's coverage (16 per sample, 64 / 63 for a whole sub-scanline, capped at
// 255) and leaves it in the pixel's first word; `sh_compose` (one invocation per layer pixel)
// paints the draws over it in order, from its 16 × 16 tile's list (Normal, as `shapes::paint` /
// `text::paint_coverage`). Draws come in groups (motion-blur sub-samples of a text layer): each
// group is composited on its own and the groups summed with their weights (`accumulate_warp`).
// A batch too large for one `acc` is split; `state` carries the pixels between batches.

struct Batch {
    // image width, image height, tiles across, rows in this batch
    n: vec4<u32>,
    // first edge, edge count, workgroups across in `sh_cov`, flags (1: groups, 2: last batch)
    e: vec4<u32>,
    // where the tile ranges and the tile lists start in `index`, workgroups across in `sh_accum`
    t: vec4<u32>,
}

struct DrawRec {
    // x0, y0, w, h of the draw's mask rectangle in layer pixels
    r: vec4<u32>,
    // acc base (words), unused, flags (1: even-odd, 2: gradient), first gradient word
    m: vec4<u32>,
    // gradient kind, group
    k: vec4<u32>,
    // straight RGBA (solid draws)
    color: vec4<f32>,
    // opacity, len2, has affine, has focal
    op: vec4<f32>,
    // layer pixel -> paint space: rows (a, c, e) and (b, d, f)
    lx: vec4<f32>,
    ly: vec4<f32>,
    // paint -> unit gradient space (affine gradients), same layout
    ax: vec4<f32>,
    ay: vec4<f32>,
    // ramp start (xy) and direction (zw)
    sd: vec4<f32>,
    // focal point (xy)
    fo: vec4<f32>,
    // group weight
    w: vec4<f32>,
}

@group(0) @binding(0) var<uniform> B: Batch;
// [first sub-scanline, (draw << 7) | (count << 1) | (winding < 0), x (16.16), dx (16.16)]
@group(0) @binding(1) var<storage, read> edges: array<vec4<i32>>;
// Per draw, per mask pixel and sub-scanline: winding deltas of its 4 sample columns, 8-bit
// lanes; after `sh_cov` the pixel's first word holds its coverage (0-255).
@group(0) @binding(2) var<storage, read_write> acc: array<atomic<i32>>;
@group(0) @binding(3) var<storage, read> draws: array<DrawRec>;
@group(0) @binding(4) var<storage, read> grad: array<f32>;
// Rows of the batch (draw index, row within it), tile ranges, tile lists (draw indices in order).
@group(0) @binding(5) var<storage, read> index: array<u32>;
// The layer's pixels between batches: the composite so far (or, with groups, the weighted sum).
@group(0) @binding(6) var<storage, read_write> state: array<vec4<f32>>;
@group(0) @binding(7) var out: texture_storage_2d<rgba32float, write>;

@compute @workgroup_size(64, 1, 1)
fn sh_accum(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let i = (wid.y * B.t.z + wid.x) * 64u + lid.x;
    if (i >= B.e.y) {
        return;
    }
    let e = edges[B.e.x + i];
    let D = draws[u32(e.y) >> 7u];
    let count = (e.y >> 1u) & 63;
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
        let at = D.m.x + ((u32(sy >> 2u) * D.r.z + u32(sx >> 2u)) << 2u) + u32(sy & 3);
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
    return grad[i];
}

fn ramp_t(D: DrawRec, p_in: vec2<f32>) -> f32 {
    var p = p_in;
    if (D.op.z > 0.5) {
        p = vec2<f32>(D.ax.x * p.x + D.ax.y * p.y + D.ax.z, D.ay.x * p.x + D.ay.y * p.y + D.ay.z);
    }
    let s = D.sd.xy;
    let d = D.sd.zw;
    let len2 = D.op.y;
    let v = p - s;
    let kind = D.k.x;
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
fn gradient_at(base: u32, t_in: f32) -> vec4<f32> {
    let t = clamp(t_in, 0.0, 1.0);
    let nc = u32(g_at(base));
    let no = u32(g_at(base + 1u));
    var col = vec4<f32>(1.0);
    if (nc > 0u) {
        let c0 = base + 2u;
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
        let o0 = base + 2u + nc * 6u;
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


// ---------------------------------------------------------------- coverage rows

// Rows are mostly glyph-sized: 64 pixels a step keeps the scan short.
const ROW: u32 = 64u;
var<workgroup> sums: array<vec4<i32>, 64>;
var<workgroup> row_at: vec4<u32>;

@compute @workgroup_size(64, 1, 1)
fn sh_cov(@builtin(local_invocation_id) lid: vec3<u32>, @builtin(workgroup_id) wid: vec3<u32>) {
    let row = wid.y * B.e.z + wid.x;
    if (row >= B.n.w) {
        return;
    }
    // The index holds (draw, row within it) per row; the draw's width, acc base and fill rule
    // are shared through the workgroup so the loop below stays uniform.
    if (lid.x == 0u) {
        let d = index[2u * row];
        let D = draws[d];
        row_at = vec4<u32>(index[2u * row + 1u], D.r.z, D.m.x, D.m.z);
    }
    let r = workgroupUniformLoad(&row_at);
    let y = r.x;
    let w = r.y;
    let base = r.z;
    let tiles = (w + ROW - 1u) / ROW;
    let even_odd = (r.w & 1u) != 0u;
    var carry = vec4<i32>(0);
    for (var t = 0u; t < tiles; t++) {
        let x = t * ROW + lid.x;
        // Per sub-scanline j: the winding deltas at this pixel's 4 sample columns.
        var dl: array<vec4<i32>, 4>;
        var total = vec4<i32>(0);
        let at = base + ((y * w + x) << 2u);
        if (x < w) {
            for (var j = 0u; j < 4u; j++) {
                let v = lanes(atomicLoad(&acc[at + j]));
                dl[j] = v;
                total[j] = v.x + v.y + v.z + v.w;
            }
        }
        sums[lid.x] = total;
        workgroupBarrier();
        for (var off = 1u; off < ROW; off = off << 1u) {
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
        carry = carry + sums[ROW - 1u];
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
            atomicStore(&acc[at], i32(min(alpha, 255u)));
        }
    }
}

// ---------------------------------------------------------------- compose

fn paint_color(D: DrawRec, bx: u32, by: u32) -> vec4<f32> {
    if ((D.m.z & 2u) == 0u) {
        return D.color;
    }
    let p = vec2<f32>(f32(bx) + 0.5, f32(by) + 0.5);
    let q = vec2<f32>(D.lx.x * p.x + D.lx.y * p.y + D.lx.z, D.ly.x * p.x + D.ly.y * p.y + D.ly.z);
    return gradient_at(D.m.w, ramp_t(D, q));
}

@compute @workgroup_size(16, 16, 1)
fn sh_compose(@builtin(global_invocation_id) gid: vec3<u32>) {
    let bx = gid.x;
    let by = gid.y;
    if (bx >= B.n.x || by >= B.n.y) {
        return;
    }
    let i = by * B.n.x + bx;
    let groups = (B.e.w & 1u) != 0u;
    // Without groups `cur` carries on from the last batch; with groups every batch starts a
    // group (batches split between groups) and `sum` carries on.
    var cur = vec4<f32>(0.0);
    var sum = vec4<f32>(0.0);
    if (groups) {
        sum = state[i];
    } else {
        cur = state[i];
    }
    var g = 0xffffffffu;
    var wt = 0.0;
    let tile = (by / 16u) * B.n.z + bx / 16u;
    let first = index[B.t.x + tile];
    let last = index[B.t.x + tile + 1u];
    for (var k = first; k < last; k++) {
        let D = draws[index[B.t.y + k]];
        if (groups && D.k.y != g) {
            sum = sum + cur * wt;
            cur = vec4<f32>(0.0);
            g = D.k.y;
            wt = D.w.x;
        }
        if (bx < D.r.x || by < D.r.y || bx - D.r.x >= D.r.z || by - D.r.y >= D.r.w) {
            continue;
        }
        let a = atomicLoad(&acc[D.m.x + (((by - D.r.y) * D.r.z + (bx - D.r.x)) << 2u)]);
        let c = f32(a) / 255.0;
        if (c <= 0.0) {
            continue;
        }
        let col = paint_color(D, bx, by);
        let al = c * D.op.x * col.a;
        if (al <= 0.0) {
            continue;
        }
        cur = vec4<f32>(col.rgb * al, al) + cur * (1.0 - al);
    }
    var px = cur;
    if (groups) {
        sum = sum + cur * wt;
        px = sum;
    }
    if ((B.e.w & 2u) != 0u) {
        textureStore(out, vec2<i32>(i32(bx), i32(by)), px);
    } else {
        state[i] = px;
    }
}
