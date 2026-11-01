// The native YUV decode of uploaded (or split, `surface.wgsl`) integer planes; the arithmetic is
// `yuv_math.wgsl`'s.
@group(0) @binding(1) var y_plane: texture_2d<u32>;
@group(0) @binding(2) var u_plane: texture_2d<u32>;
@group(0) @binding(3) var v_plane: texture_2d<u32>;
@group(0) @binding(4) var a_plane: texture_2d<u32>;
@group(0) @binding(5) var out: texture_storage_2d<rgba32float, write>;
@group(0) @binding(6) var rgba: texture_2d<f32>;
@group(0) @binding(7) var coverage: texture_2d<f32>;

fn load_y(pos: vec2<i32>) -> u32 { return textureLoad(y_plane, pos, 0).r; }
fn load_chroma(k: u32, pos: vec2<i32>) -> u32 {
    if k == 1u { return textureLoad(u_plane, pos, 0).r; }
    return textureLoad(v_plane, pos, 0).r;
}
fn chroma_dims(k: u32) -> vec2<i32> {
    if k == 1u { return vec2<i32>(textureDimensions(u_plane)); }
    return vec2<i32>(textureDimensions(v_plane));
}
fn load_alpha(pos: vec2<i32>) -> u32 { return textureLoad(a_plane, pos, 0).r; }

@compute @workgroup_size(16, 16)
fn decode(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(out)) { return; }
    textureStore(out, vec2<i32>(id.xy), decode_px(id.xy));
}
@compute @workgroup_size(16, 16)
fn mask(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= textureDimensions(out)) { return; }
    let pos = vec2<i32>(id.xy);
    textureStore(out, pos, textureLoad(rgba, pos, 0) * textureLoad(coverage, pos, 0).r);
}
