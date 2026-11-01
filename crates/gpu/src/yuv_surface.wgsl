// The native YUV decode reading a hardware decoder's surface directly (Fix 3c, 1 Nov 2026): each
// sample is made from the surface as `surface.wgsl`'s split makes it (`surface_samples.wgsl`) and
// goes through the same arithmetic (`yuv_math.wgsl`), so the picture is the split + decode's,
// bit for bit, without writing and reading back the split planes.
@group(0) @binding(1) var surface_luma: texture_2d<u32>;
@group(0) @binding(2) var surface_chroma: texture_2d<u32>;
@group(0) @binding(3) var surface_alpha: texture_2d<u32>;
@group(0) @binding(4) var<uniform> split_params: Split;
@group(0) @binding(5) var out: texture_storage_2d<rgba32float, write>;

fn origin() -> vec2<i32> { return vec2<i32>(i32(split_params.x), i32(split_params.y)); }
fn load_y(pos: vec2<i32>) -> u32 { return eight(down(textureLoad(surface_luma, pos + origin(), 0).r)); }
fn load_chroma(k: u32, pos: vec2<i32>) -> u32 {
    let at = pos + vec2<i32>(i32(split_params.x >> split_params.sx), i32(split_params.y >> split_params.sy));
    let c = textureLoad(surface_chroma, at, 0);
    if k == 1u { return eight(down(c.r)); }
    return eight(down(c.g));
}
// (the split's chroma planes are the picture's chroma size)
fn chroma_dims(k: u32) -> vec2<i32> { return vec2<i32>(i32(split_params.cw), i32(split_params.ch)); }
fn load_alpha(pos: vec2<i32>) -> u32 { return eight_alpha(textureLoad(surface_alpha, pos + origin(), 0).r >> split_params.shift); }

@compute @workgroup_size(16, 16)
fn decode_surface(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(out)) { return; }
    textureStore(out, vec2<i32>(id.xy), decode_px(id.xy));
}
