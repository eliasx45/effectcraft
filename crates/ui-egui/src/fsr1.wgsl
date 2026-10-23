// UP1: AMD FidelityFX Super Resolution 1.0 (EASU upscale + RCAS sharpen) for the Composition
// viewer's low-resolution previews (crates/ui-egui/src/fsr_upscale.rs). Ported to WGSL from
// FsrEasuF and FsrRcasF in ffx_fsr1.h (https://github.com/GPUOpen-Effects/FidelityFX-FSR),
// by way of the Metal port in the M3 preview-upscale experiment. AMD's licence:
//
// Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.
//
// Both passes work on the viewer frame's premultiplied, sRGB-encoded RGBA8 values (the
// perceptual space FSR1 expects). Alpha is filtered with the colour (EASU) or kept (RCAS), and
// colour never exceeds alpha, so the result stays valid premultiplied RGBA.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;

// RCAS sharpness 0.2 stops: exp2(-0.2).
const RCAS_SHARP: f32 = 0.8705506;
const RCAS_LIMIT: f32 = 0.1875; // 0.25 - 1/16

// Texel `p` of level 0, clamped to the edge.
fn ld(p: vec2<i32>) -> vec4<f32> {
    let m = vec2<i32>(textureDimensions(src, 0)) - vec2<i32>(1);
    return textureLoad(src, clamp(p, vec2<i32>(0), m), 0);
}

fn rcpsafe(x: f32) -> f32 {
    return 1.0 / max(x, 1.0e-8);
}

// FSR's cheap luma (times 2).
fn luma2(c: vec4<f32>) -> f32 {
    return c.b * 0.5 + (c.r * 0.5 + c.g);
}

// FsrEasuSetF: one bilinear corner's direction (xy) and edge length (z), weighted by `w`.
//    a
//  b c d
//    e
fn easu_set(w: f32, la: f32, lb: f32, lc: f32, ld_: f32, le: f32) -> vec3<f32> {
    let dc = ld_ - lc;
    let cb = lc - lb;
    let dir_x = ld_ - lb;
    var len_x = saturate(abs(dir_x) * rcpsafe(max(abs(dc), abs(cb))));
    len_x = len_x * len_x;
    let ec = le - lc;
    let ca = lc - la;
    let dir_y = le - la;
    var len_y = saturate(abs(dir_y) * rcpsafe(max(abs(ec), abs(ca))));
    len_y = len_y * len_y;
    return vec3<f32>(dir_x * w, dir_y * w, (len_x + len_y) * w);
}

// FsrEasuTapF: the weight of the tap at offset `off`.
fn easu_w(off: vec2<f32>, dir: vec2<f32>, len: vec2<f32>, lob: f32, clp: f32) -> f32 {
    var v = vec2<f32>(off.x * dir.x + off.y * dir.y, off.x * (-dir.y) + off.y * dir.x);
    v = v * len;
    let d2 = min(v.x * v.x + v.y * v.y, clp);
    var wb = (2.0 / 5.0) * d2 - 1.0;
    var wa = lob * d2 - 1.0;
    wb = wb * wb;
    wa = wa * wa;
    wb = (25.0 / 16.0) * wb - (25.0 / 16.0 - 1.0);
    return wb * wa;
}

// Edge-Adaptive Spatial Upsampling: `src` (the frame) to `dst` (the display size).
@compute @workgroup_size(8, 8, 1)
fn easu(@builtin(global_invocation_id) gid: vec3<u32>) {
    let od = textureDimensions(dst);
    if gid.x >= od.x || gid.y >= od.y {
        return;
    }
    let s = vec2<f32>(textureDimensions(src, 0)) / vec2<f32>(od);
    var pp = vec2<f32>(gid.xy) * s + (0.5 * s - 0.5);
    let fp = floor(pp);
    pp = pp - fp;
    let i = vec2<i32>(fp);
    // 12-tap kernel.
    //    b c
    //  e f g h
    //  i j k l
    //    n o
    let b = ld(i + vec2<i32>(0, -1));
    let c = ld(i + vec2<i32>(1, -1));
    let e = ld(i + vec2<i32>(-1, 0));
    let f = ld(i);
    let g = ld(i + vec2<i32>(1, 0));
    let h = ld(i + vec2<i32>(2, 0));
    let ii = ld(i + vec2<i32>(-1, 1));
    let j = ld(i + vec2<i32>(0, 1));
    let k = ld(i + vec2<i32>(1, 1));
    let l = ld(i + vec2<i32>(2, 1));
    let n = ld(i + vec2<i32>(0, 2));
    let o = ld(i + vec2<i32>(1, 2));
    let bl = luma2(b);
    let cl = luma2(c);
    let el = luma2(e);
    let fl = luma2(f);
    let gl = luma2(g);
    let hl = luma2(h);
    let il = luma2(ii);
    let jl = luma2(j);
    let kl = luma2(k);
    let ll = luma2(l);
    let nl = luma2(n);
    let ol = luma2(o);
    var acc = easu_set((1.0 - pp.x) * (1.0 - pp.y), bl, el, fl, gl, jl);
    acc = acc + easu_set(pp.x * (1.0 - pp.y), cl, fl, gl, hl, kl);
    acc = acc + easu_set((1.0 - pp.x) * pp.y, fl, il, jl, kl, nl);
    acc = acc + easu_set(pp.x * pp.y, gl, jl, kl, ll, ol);
    var dir = acc.xy;
    var len = acc.z;
    let dir2 = dir * dir;
    var dir_r = dir2.x + dir2.y;
    let zro = dir_r < (1.0 / 32768.0);
    dir_r = inverseSqrt(max(dir_r, 1.0e-12));
    if zro {
        dir_r = 1.0;
        dir.x = 1.0;
    }
    dir = dir * dir_r;
    len = len * 0.5;
    len = len * len;
    let stretch = (dir.x * dir.x + dir.y * dir.y) * rcpsafe(max(abs(dir.x), abs(dir.y)));
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = rcpsafe(lob);
    let min4 = min(min(f, g), min(j, k));
    let max4 = max(max(f, g), max(j, k));
    var ac = vec4<f32>(0.0);
    var aw = 0.0;
    var w = easu_w(vec2<f32>(0.0, -1.0) - pp, dir, len2, lob, clp);
    ac = ac + b * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(1.0, -1.0) - pp, dir, len2, lob, clp);
    ac = ac + c * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(-1.0, 1.0) - pp, dir, len2, lob, clp);
    ac = ac + ii * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(0.0, 1.0) - pp, dir, len2, lob, clp);
    ac = ac + j * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(0.0, 0.0) - pp, dir, len2, lob, clp);
    ac = ac + f * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(-1.0, 0.0) - pp, dir, len2, lob, clp);
    ac = ac + e * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(1.0, 1.0) - pp, dir, len2, lob, clp);
    ac = ac + k * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(2.0, 1.0) - pp, dir, len2, lob, clp);
    ac = ac + l * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(2.0, 0.0) - pp, dir, len2, lob, clp);
    ac = ac + h * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(1.0, 0.0) - pp, dir, len2, lob, clp);
    ac = ac + g * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(1.0, 2.0) - pp, dir, len2, lob, clp);
    ac = ac + o * w;
    aw = aw + w;
    w = easu_w(vec2<f32>(0.0, 2.0) - pp, dir, len2, lob, clp);
    ac = ac + n * w;
    aw = aw + w;
    // Deringing: within the 4 nearest texels. A degenerate weight sum takes the nearest texel.
    var pix = f;
    if abs(aw) > 1.0e-8 {
        pix = clamp(ac / aw, min4, max4);
    }
    pix = vec4<f32>(min(pix.rgb, vec3<f32>(pix.a)), pix.a);
    textureStore(dst, vec2<i32>(gid.xy), pix);
}

// Robust Contrast-Adaptive Sharpening at the output size (`src` and `dst` are the same size).
//    b
//  d e f
//    h
@compute @workgroup_size(8, 8, 1)
fn rcas(@builtin(global_invocation_id) gid: vec3<u32>) {
    let od = textureDimensions(dst);
    if gid.x >= od.x || gid.y >= od.y {
        return;
    }
    let p = vec2<i32>(gid.xy);
    let b = ld(p + vec2<i32>(0, -1)).rgb;
    let d = ld(p + vec2<i32>(-1, 0)).rgb;
    let e4 = ld(p);
    let e = e4.rgb;
    let f = ld(p + vec2<i32>(1, 0)).rgb;
    let h = ld(p + vec2<i32>(0, 1)).rgb;
    let mn4 = min(min(b, d), min(f, h));
    let mx4 = max(max(b, d), max(f, h));
    let hit_min = min(mn4, e) / (4.0 * mx4 + 1.0e-8);
    let hit_max = (1.0 - max(mx4, e)) / (4.0 * mn4 - 4.0 - 1.0e-8);
    let lobe_rgb = max(-hit_min, hit_max);
    let lobe = max(-RCAS_LIMIT, min(max(lobe_rgb.r, max(lobe_rgb.g, lobe_rgb.b)), 0.0)) * RCAS_SHARP;
    let rcp_l = 1.0 / (4.0 * lobe + 1.0);
    let pix = (lobe * (b + d + h + f) + e) * rcp_l;
    textureStore(dst, p, vec4<f32>(min(saturate(pix), vec3<f32>(e4.a)), e4.a));
}
