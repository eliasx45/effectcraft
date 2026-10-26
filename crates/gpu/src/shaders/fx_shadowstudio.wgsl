// GPU Shadow Studio 2 / 3: see src/fx_shadowstudio.rs. Each kernel mirrors
// effects::shadow_studio's per-pixel steps operation for operation.

// The cast: the most opaque step reaching this padded pixel, `colour · a, a`. src is the
// unpadded layer at (left, top) in the output; data = (x shift, y shift, opacity) per step.
// u[0] = (steps, left, top); f[0] = colour; f[1].x = alpha threshold.
@compute @workgroup_size(16, 16)
fn fss_cast(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= d.x || p.y >= d.y) {
        return;
    }
    let ox = p.x - i32(P.u[0].y);
    let oy = p.y - i32(P.u[0].z);
    let threshold = P.f[1].x;
    var m = 0.0;
    for (var i = 0u; i < P.u[0].x; i = i + 1u) {
        let k = 3u * i;
        let s = tex_get(src, ox - i32(data[k]), oy - i32(data[k + 1u])).w;
        if (s > threshold) {
            let a = s * data[k + 2u];
            if (a > m) {
                m = a;
            }
        }
    }
    let c = P.f[0];
    textureStore(out, p, vec4<f32>(c.x * m, c.y * m, c.z * m, m));
}

// The source (aux, at (left, top), times Source Opacity) over its shadow (src).
// u[0] = (left, top); f[0].x = source opacity.
@compute @workgroup_size(16, 16)
fn fss_over(@builtin(global_invocation_id) gid: vec3<u32>) {
    let d = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= d.x || p.y >= d.y) {
        return;
    }
    let s = textureLoad(src, p, 0);
    let q = p - vec2<i32>(i32(P.u[0].x), i32(P.u[0].y));
    let ad = vec2<i32>(textureDimensions(aux));
    if (q.x < 0 || q.y < 0 || q.x >= ad.x || q.y >= ad.y) {
        textureStore(out, p, s);
        return;
    }
    let source = textureLoad(aux, q, 0) * P.f[0].x;
    let behind = 1.0 - source.w;
    textureStore(out, p, vec4<f32>(source.xyz + s.xyz * behind, source.w + s.w * behind));
}
