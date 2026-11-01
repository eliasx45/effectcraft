// A hardware decoder's surface samples as the platform decoder copies them for the CPU (shared by
// the split into planes, `surface.wgsl`, and the decode that reads the surface directly,
// `yuv_surface.wgsl`). Each module declares `split_params: Split`.
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
