// Mask feathers (masks_gpu.rs): a coverage plane in and out of the RGBA working images the
// shared box-blur kernels (`box_h`, `box_v`) run on.

struct MaskFeatherP {
    // The crop window's top-left in the blurred plane, and its size (pack, combine).
    off: vec2<u32>,
    size: vec2<u32>,
    // combine: the mask's mode (0 Add, 1 Subtract, 2 Intersect, 3 Lighten, 4 Darken,
    // 5 Difference), inverted, opacity; the first mask combines with `init`, not `mf_aux`.
    mode: u32,
    inverted: u32,
    opacity: f32,
    first: u32,
    init: f32,
    p0: u32,
    p1: u32,
    p2: u32,
};

@group(0) @binding(0) var<uniform> mfp: MaskFeatherP;
@group(0) @binding(1) var mf_src: texture_2d<f32>;
@group(0) @binding(2) var mf_dst: texture_storage_2d<rgba32float, write>;
// combine: the accumulator so far; apply: the combined coverage. Others bind `mf_src` again.
@group(0) @binding(3) var mf_aux: texture_2d<f32>;

// The uploaded R32Float coverage plane into every channel of a working image (same size).
@compute @workgroup_size(16, 16)
fn mask_expand(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = textureDimensions(mf_dst);
    if (gid.x >= d.x || gid.y >= d.y) {
        return;
    }
    let v = textureLoad(mf_src, vec2<i32>(gid.xy), 0).r;
    textureStore(mf_dst, vec2<i32>(gid.xy), vec4<f32>(v));
}

// The crop window of the blurred image's alpha, four coverage values a texel (x = 4i..4i+3 of
// row y in texel (i, y); past the window's width 0), so the readback is the plane's own bytes.
@compute @workgroup_size(16, 16)
fn mask_pack(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = textureDimensions(mf_dst);
    if (gid.x >= d.x || gid.y >= d.y) {
        return;
    }
    var o = vec4<f32>(0.0);
    for (var k = 0u; k < 4u; k++) {
        let x = gid.x * 4u + k;
        if (x < mfp.size.x) {
            o[k] = textureLoad(mf_src, vec2<i32>(i32(x + mfp.off.x), i32(gid.y + mfp.off.y)), 0).a;
        }
    }
    textureStore(mf_dst, vec2<i32>(gid.xy), o);
}

// One mask into the accumulator (`masks::combined`): its blurred coverage's alpha at the layer
// pixel (the plane is padded by `off`), inverted, times its opacity, combined by its mode.
@compute @workgroup_size(16, 16)
fn mask_combine(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = textureDimensions(mf_dst);
    if (gid.x >= d.x || gid.y >= d.y) {
        return;
    }
    var c = textureLoad(mf_src, vec2<i32>(i32(gid.x + mfp.off.x), i32(gid.y + mfp.off.y)), 0).a;
    if (mfp.inverted != 0u) {
        c = 1.0 - c;
    }
    c = c * mfp.opacity;
    var a = mfp.init;
    if (mfp.first == 0u) {
        a = textureLoad(mf_aux, vec2<i32>(gid.xy), 0).a;
    }
    var r = a;
    switch mfp.mode {
        case 0u: { r = a + c - a * c; }
        case 1u: { r = a * (1.0 - c); }
        case 2u: { r = a * c; }
        case 3u: { r = max(a, c); }
        case 4u: { r = min(a, c); }
        case 5u: { r = abs(a - c); }
        default: {}
    }
    textureStore(mf_dst, vec2<i32>(gid.xy), vec4<f32>(r));
}

// The layer image times the combined coverage, every channel (`Image::mul_mask`).
@compute @workgroup_size(16, 16)
fn mask_apply(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = textureDimensions(mf_dst);
    if (gid.x >= d.x || gid.y >= d.y) {
        return;
    }
    let m = textureLoad(mf_aux, vec2<i32>(gid.xy), 0).a;
    textureStore(mf_dst, vec2<i32>(gid.xy), textureLoad(mf_src, vec2<i32>(gid.xy), 0) * m);
}
