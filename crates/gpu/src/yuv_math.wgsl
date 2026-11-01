// The native YUV decode's arithmetic (`yuv.rs`), shared by the decode of uploaded planes
// (`yuv.wgsl`) and the one that reads a hardware decoder's surface directly (`yuv_surface.wgsl`,
// Fix 3c): the same IEEE-exact operations on the same integer samples, so both give the same
// floats. Each module defines where the samples come from: `load_y(pos)`, `load_chroma(k, pos)`
// (k 1: Cb, 2: Cr), `chroma_dims(k)` (the chroma plane's size) and `load_alpha(pos)`.
struct Params { shape: vec4<u32>, range: vec4<f32>, rgb: vec4<f32>, alpha: vec4<f32> }
@group(0) @binding(0) var<uniform> p: Params;

fn vertical(k: u32, x: i32, y: i32, other: i32, weight: f32) -> u32 {
    let a = bitcast<u32>(f32(load_chroma(k, vec2<i32>(x, y))));
    if p.shape.y == 0u { return ieee_mul_bits(ieee_sub_bits(a,bitcast<u32>(p.range.z)),bitcast<u32>(p.range.w)); }
    let b = bitcast<u32>(f32(load_chroma(k, vec2<i32>(x, other))));
    let first=ieee_mul_bits(a,bitcast<u32>(1.0-weight));
    let second=ieee_mul_bits(b,bitcast<u32>(weight));
    let interpolated=ieee_add_bits(first,second);
    return ieee_mul_bits(ieee_sub_bits(interpolated,bitcast<u32>(p.range.z)),bitcast<u32>(p.range.w));
}
fn chroma(k: u32, pos: vec2<u32>) -> u32 {
    let dims = chroma_dims(k);
    let center = min(vec2<i32>(pos >> p.shape.xy), dims - 1);
    let neighbor = clamp(center + vec2<i32>(select(-1, 1, pos.x % 2u == 1u), select(-1, 1, pos.y % 2u == 1u)), vec2<i32>(0), dims - 1);
    let weight = select(0.0, 0.25, p.shape.y == 1u);
    let a = vertical(k, center.x, center.y, neighbor.y, weight);
    if p.shape.x == 0u { return a; }
    let b = vertical(k, neighbor.x, center.y, neighbor.y, weight);
    return ieee_add_bits(ieee_mul_bits(a,bitcast<u32>(0.75)),ieee_mul_bits(b,bitcast<u32>(0.25)));
}
// The premultiplied RGBA of the picture's pixel `id`.
fn decode_px(id: vec2<u32>) -> vec4<f32> {
    let pos = vec2<i32>(id);
    let y = ieee_mul_bits(ieee_sub_bits(bitcast<u32>(f32(load_y(pos))), bitcast<u32>(p.range.x)), bitcast<u32>(p.range.y));
    let u = chroma(1u, id);
    let v = chroma(2u, id);
    let red=ieee_add_bits(y,ieee_mul_bits(bitcast<u32>(p.rgb.x),v));
    let green=ieee_sub_bits(ieee_sub_bits(y,ieee_mul_bits(bitcast<u32>(p.rgb.y),v)),ieee_mul_bits(bitcast<u32>(p.rgb.z),u));
    let blue=ieee_add_bits(y,ieee_mul_bits(bitcast<u32>(p.rgb.w),u));
    var rgb = clamp(bitcast<vec3<f32>>(vec3<u32>(red,green,blue)), vec3<f32>(0.0), vec3<f32>(1.0));
    var a = 1.0;
    if p.shape.z != 0u && p.shape.w != 0u {
        a = clamp(f32(load_alpha(pos)) * p.alpha.w, 0.0, 1.0);
        if p.shape.w == 1u { rgb *= a; }
        else { rgb = max(rgb - p.alpha.xyz * (1.0 - a), vec3<f32>(0.0)); }
    }
    return vec4<f32>(rgb, a);
}
