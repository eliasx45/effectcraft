// A decoder's biplanar 4:2:0 surface (Y, interleaved CbCr; 8-bit, or 10-bit in the high bits of
// 16-bit words) split into the three integer planes `yuv.wgsl`'s `decode` reads (surface.rs).

struct Split {
    shift: u32,
    width: u32,
    height: u32,
    pad: u32,
    // the picture's origin in the surface (even: 4:2:0 crops are)
    x: u32,
    y: u32,
    pad2: u32,
    pad3: u32,
}

@group(0) @binding(0) var<uniform> split_params: Split;
@group(0) @binding(1) var split_luma: texture_2d<u32>;
@group(0) @binding(2) var split_chroma: texture_2d<u32>;
@group(0) @binding(3) var split_y: texture_storage_2d<r32uint, write>;
@group(0) @binding(4) var split_u: texture_storage_2d<r32uint, write>;
@group(0) @binding(5) var split_v: texture_storage_2d<r32uint, write>;

@compute @workgroup_size(16, 16)
fn split(@builtin(global_invocation_id) id: vec3<u32>) {
    let p = vec2<i32>(id.xy);
    if (id.x < split_params.width && id.y < split_params.height) {
        let y = textureLoad(split_luma, p + vec2<i32>(i32(split_params.x), i32(split_params.y)), 0).r >> split_params.shift;
        textureStore(split_y, p, vec4<u32>(y, 0u, 0u, 0u));
    }
    let cw = (split_params.width + 1u) / 2u;
    let ch = (split_params.height + 1u) / 2u;
    if (id.x < cw && id.y < ch) {
        let c = textureLoad(split_chroma, p + vec2<i32>(i32(split_params.x / 2u), i32(split_params.y / 2u)), 0);
        textureStore(split_u, p, vec4<u32>(c.r >> split_params.shift, 0u, 0u, 0u));
        textureStore(split_v, p, vec4<u32>(c.g >> split_params.shift, 0u, 0u, 0u));
    }
}
