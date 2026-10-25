// Mask feathers (masks_gpu.rs): a coverage plane in and out of the RGBA working images the
// shared box-blur kernels (`box_h`, `box_v`) run on.

struct MaskFeatherP {
    // The crop window's top-left in the blurred plane, and its size (pack only).
    off: vec2<u32>,
    size: vec2<u32>,
};

@group(0) @binding(0) var<uniform> mfp: MaskFeatherP;
@group(0) @binding(1) var mf_src: texture_2d<f32>;
@group(0) @binding(2) var mf_dst: texture_storage_2d<rgba32float, write>;

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
