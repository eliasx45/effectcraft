// A decoder's surface (a Y plane, an interleaved CbCr plane and, for ProRes 4444, an alpha plane;
// 8-bit, or 16-bit words holding the coded depth in their high bits) split into the integer planes
// `yuv.wgsl`'s `decode` reads (surface.rs). Exactly the samples the platform decoder copies for the
// CPU: luma / chroma either shifted down (H.264 / HEVC: the low bits are zero) or rounded to the
// nearest code and clamped (ProRes: VideoToolbox keeps more precision than the coded depth);
// alpha always shifted down (its low bits repeat the high ones).

struct Split {
    shift: u32,
    width: u32,
    height: u32,
    // 1: luma / chroma round to nearest (and clamp) instead of shifting down
    round: u32,
    // the picture's origin in the surface (a multiple of the chroma subsampling)
    x: u32,
    y: u32,
    // chroma plane size of the picture and its subsampling shifts
    cw: u32,
    ch: u32,
    sx: u32,
    sy: u32,
    // 1: write the alpha plane
    alpha: u32,
    // 10 / 12: round the samples (at this depth) to 8 bits, as the media pool's `frame_to_8bit`
    // does (limited-range Y'CbCr; alpha is full range); 0: keep them
    to8: u32,
}

@group(0) @binding(0) var<uniform> split_params: Split;
@group(0) @binding(1) var split_luma: texture_2d<u32>;
@group(0) @binding(2) var split_chroma: texture_2d<u32>;
@group(0) @binding(3) var split_y: texture_storage_2d<r32uint, write>;
@group(0) @binding(4) var split_u: texture_storage_2d<r32uint, write>;
@group(0) @binding(5) var split_v: texture_storage_2d<r32uint, write>;
@group(0) @binding(6) var split_alpha_in: texture_2d<u32>;
@group(0) @binding(7) var split_a: texture_storage_2d<r32uint, write>;

fn down(v: u32) -> u32 {
    let s = split_params.shift;
    if (split_params.round == 0u) {
        return v >> s;
    }
    // ((v + half) >> s), at most the largest code of 16 - s bits (convert.rs round_down)
    let half = (1u << s) >> 1u;
    return min((v + half) >> s, 0xffffu >> s);
}

// a limited-range Y'CbCr sample at `to8` bits, rounded to 8
fn eight(v: u32) -> u32 {
    let b = split_params.to8;
    if (b == 0u) {
        return v;
    }
    let max = (1u << b) - 1u;
    let s = b - 8u;
    return min((min(v, max) + ((1u << s) >> 1u)) >> s, 255u);
}

// a full-range alpha sample at `to8` bits, scaled to 8
fn eight_alpha(v: u32) -> u32 {
    let b = split_params.to8;
    if (b == 0u) {
        return v;
    }
    let max = (1u << b) - 1u;
    return min((min(v, max) * 255u + max / 2u) / max, 255u);
}

@compute @workgroup_size(16, 16)
fn split(@builtin(global_invocation_id) id: vec3<u32>) {
    let p = vec2<i32>(id.xy);
    if (id.x < split_params.width && id.y < split_params.height) {
        let at = p + vec2<i32>(i32(split_params.x), i32(split_params.y));
        let y = eight(down(textureLoad(split_luma, at, 0).r));
        textureStore(split_y, p, vec4<u32>(y, 0u, 0u, 0u));
        if (split_params.alpha != 0u) {
            let a = eight_alpha(textureLoad(split_alpha_in, at, 0).r >> split_params.shift);
            textureStore(split_a, p, vec4<u32>(a, 0u, 0u, 0u));
        }
    }
    if (id.x < split_params.cw && id.y < split_params.ch) {
        let at = p + vec2<i32>(i32(split_params.x >> split_params.sx), i32(split_params.y >> split_params.sy));
        let c = textureLoad(split_chroma, at, 0);
        textureStore(split_u, p, vec4<u32>(eight(down(c.r)), 0u, 0u, 0u));
        textureStore(split_v, p, vec4<u32>(eight(down(c.g)), 0u, 0u, 0u));
    }
}
