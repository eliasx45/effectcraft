//! Classic 3D runs on the GPU (`classic3d.wgsl`): the CPU prepares the planes
//! ([`Renderer::prepare_3d_run`](effectcraft_render::Renderer::prepare_3d_run): layer buffers,
//! homographies, materials, lights, track mattes, bokeh depth of field), the GPU packs the
//! buffers into one atlas texture and runs the per-pixel depth sort, lighting, ray-cast shadows
//! and blending of `render::three_d::compose` in one kernel.

use std::collections::HashMap;

use effectcraft_geom::Mat4;
use effectcraft_project::LightKind;
use effectcraft_render::three_d::{LightState, PlaneGeo, Run3d};

use crate::context::{Enc, GpuImage, Params};
use crate::ops::mode_id;

/// Fragments kept per pixel (`MAX_FRAGS` in the shader). Runs that could stack more planes on
/// one pixel render on the CPU.
const MAX_FRAGS: usize = 32;

/// Little-endian words for the shader's storage structs.
#[derive(Default)]
struct Words(Vec<u8>);

impl Words {
    fn f(&mut self, v: [f32; 4]) {
        for x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
    fn u(&mut self, v: [u32; 4]) {
        for x in v {
            self.0.extend_from_slice(&x.to_le_bytes());
        }
    }
    fn d(&mut self, v: [f64; 4]) {
        self.f(v.map(|x| x as f32));
    }
    /// Pad an empty array to one element (`size` bytes) so its binding is valid.
    fn at_least(mut self, size: usize) -> Vec<u8> {
        if self.0.len() < size {
            self.0.resize(size, 0);
        }
        self.0
    }
}

fn affine_rows(w: &mut Words, m: &Mat4) {
    for r in 0..3 {
        w.d(m.0[r]);
    }
}

/// Shelf-pack rectangles into an atlas no larger than `max` on a side: (width, height,
/// positions).
fn pack(sizes: &[(u32, u32)], max: u32) -> Option<(u32, u32, Vec<(u32, u32)>)> {
    let maxw = sizes.iter().map(|s| s.0).max()?;
    if maxw > max {
        return None;
    }
    let area: f64 = sizes.iter().map(|s| s.0 as f64 * s.1 as f64).sum();
    let limit = maxw.max((area.sqrt() * 1.25).ceil() as u32).min(max);
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sizes[i].1));
    let mut pos = vec![(0, 0); sizes.len()];
    let (mut x, mut y, mut shelf, mut width) = (0u32, 0u32, 0u32, 0u32);
    for i in order {
        let (w, h) = sizes[i];
        if x + w > limit {
            y += shelf;
            x = 0;
            shelf = 0;
        }
        pos[i] = (x, y);
        x += w;
        shelf = shelf.max(h);
        width = width.max(x);
    }
    let height = y + shelf;
    (height <= max).then_some((width.max(1), height.max(1), pos))
}

/// Upper bound of the planes drawn on any one pixel: a pixel covered by a set of planes lies in
/// each one's box, so every plane of the set overlaps the box of any member.
fn max_overlap(boxes: &[[i64; 4]]) -> usize {
    boxes.iter().map(|a| boxes.iter().filter(|b| a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]).count()).max().unwrap_or(0)
}

fn light_kind(k: LightKind) -> u32 {
    match k {
        LightKind::Ambient | LightKind::Environment => 0,
        LightKind::Parallel => 1,
        LightKind::Point => 2,
        LightKind::Spot => 3,
    }
}

fn write_geo(w: &mut Words, g: &PlaneGeo) {
    // The homography is defined up to scale: normalise it (by a positive factor, keeping the
    // sign tests) into f32's comfortable range.
    let m = g.hinv.0.iter().flatten().fold(0.0f64, |a, v| a.max(v.abs()));
    let k = if m > 0.0 { 1.0 / m } else { 1.0 };
    for r in 0..3 {
        w.d([g.hinv.0[r][0] * k, g.hinv.0[r][1] * k, g.hinv.0[r][2] * k, 0.0]);
    }
    w.d([g.depth[0], g.depth[1], g.depth[2], 0.0]);
    affine_rows(w, &g.world);
    w.d([g.normal.x, g.normal.y, g.normal.z, 0.0]);
    w.d([g.cam.eye.x, g.cam.eye.y, g.cam.eye.z, if g.cam.ortho { 1.0 } else { 0.0 }]);
    let f = g.cam.forward();
    w.d([f.x, f.y, f.z, 0.0]);
    // Sub-sample depth along the frame camera's ray, (a·s) / (b·s), normalised like the
    // homography (w = 1: present).
    match g.ray_depth {
        Some([a, b]) => {
            let m = a.iter().chain(&b).fold(0.0f64, |m, v| m.max(v.abs()));
            let k = if m > 0.0 { 1.0 / m } else { 1.0 };
            w.d([a[0] * k, a[1] * k, a[2] * k, 1.0]);
            w.d([b[0] * k, b[1] * k, b[2] * k, 0.0]);
        }
        None => {
            w.d([0.0; 4]);
            w.d([0.0; 4]);
        }
    }
}

fn write_light(w: &mut Words, l: &LightState) {
    w.u([light_kind(l.kind), l.falloff, l.casts_shadows as u32, 0]);
    w.d([l.pos.x, l.pos.y, l.pos.z, 0.0]);
    w.d([l.dir.x, l.dir.y, l.dir.z, 0.0]);
    w.f([l.color[0], l.color[1], l.color[2], 0.0]);
    w.d([l.cone_half, l.feather, l.radius, l.falloff_distance]);
    w.d([l.shadow_darkness, l.shadow_diffusion, 0.0, 0.0]);
}

const PLANE_BYTES: usize = 12 * 16;
const GEO_BYTES: usize = 12 * 16;
const LIGHT_BYTES: usize = 6 * 16;

/// Composite a prepared Classic 3D run over `canvas`. `None` when it cannot run here (the
/// caller draws it on the CPU).
#[cfg(test)]
pub(crate) fn draw_run(e: &mut Enc, run: &Run3d, canvas: &GpuImage) -> Option<GpuImage> {
    draw_run_made(e, run, canvas, &HashMap::new())
}

/// [`draw_run`] with planes the GPU made itself (`crate::plane3d`): their images by the
/// address of their (pixel-less) plane buffers.
pub(crate) fn draw_run_made(e: &mut Enc, run: &Run3d, canvas: &GpuImage, made: &HashMap<usize, GpuImage>) -> Option<GpuImage> {
    let (w, h) = (canvas.width, canvas.height);
    let boxes: Vec<[i64; 4]> = run.planes.iter().filter(|p| p.draw).map(|p| p.bbox).collect();
    if boxes.is_empty() {
        return Some(canvas.clone());
    }
    if max_overlap(&boxes) > MAX_FRAGS
        || run.planes.iter().any(|p| p.winv.0[3] != [0.0, 0.0, 0.0, 1.0] || p.geos.iter().any(|g| g.world.0[3] != [0.0, 0.0, 0.0, 1.0]))
    {
        return None;
    }
    // LVR1: an 8 bpc run keeps its planes as codes, in an atlas of codes: a quarter of the f32
    // atlas, and no f32 copy of each plane first (N2's 14 glow planes: an 809 MB f32 atlas plus
    // 809 MB of decoded planes per frame ran an 8 GB card out of memory). The kernel decodes each
    // texel to the floats the f32 atlas would hold (`codes::classic3d_source`): the same pixels.
    let (sizes, atlas) = match codes_planes(e, run, made) {
        Some(parts) => {
            let sizes: Vec<(u32, u32)> = parts.iter().map(|(c, _)| (c.width(), c.height())).collect();
            let (aw, ah, pos) = pack(&sizes, e.g.max_dim)?;
            if (aw as u64) * (ah as u64) * 4 > 2 << 30 {
                return None;
            }
            let placed: Vec<_> = parts.iter().map(|(c, _)| c).zip(pos.iter().copied()).collect();
            let atlas = crate::codes::atlas(e, &placed, aw, ah)?;
            (parts.into_iter().map(|(c, off)| ((c.width(), c.height()), off)).zip(pos).collect::<Vec<_>>(), Atlas::Codes(atlas))
        }
        None => {
            // Plane textures: the layer uploads (cached per buffer), with bokeh depth of field
            // blurred here when the plane has it.
            let mut texs: Vec<(GpuImage, [f64; 2])> = Vec::with_capacity(run.planes.len());
            for p in &run.planes {
                let up = match made.get(&(std::sync::Arc::as_ptr(&p.buf) as usize)) {
                    Some(img) => img.clone(),
                    None => e.upload_buf(&p.buf)?,
                };
                match &p.dof {
                    Some(d) => {
                        let pad = d.pad as f64;
                        texs.push((crate::bokeh::apply(e, &up, d)?, [p.buf.offset[0] + pad, p.buf.offset[1] + pad]));
                    }
                    None => texs.push((up, p.buf.offset)),
                }
            }
            let sizes: Vec<(u32, u32)> = texs.iter().map(|(t, _)| (t.width, t.height)).collect();
            let (aw, ah, pos) = pack(&sizes, e.g.max_dim)?;
            if (aw as u64) * (ah as u64) * 16 > 2 << 30 {
                return None;
            }
            let atlas = e.image(aw, ah);
            for ((t, _), &(x, y)) in texs.iter().zip(&pos) {
                e.copy_into(t, &atlas, x, y);
            }
            (texs.into_iter().map(|(t, off)| ((t.width, t.height), off)).zip(pos).collect::<Vec<_>>(), Atlas::F32(atlas))
        }
    };
    // Track mattes (shared by the planes of one layer).
    let mut mattes: Vec<f32> = vec![];
    let mut matte_ix: HashMap<usize, u32> = HashMap::new();
    let n_px = w as usize * h as usize;
    let mut planes = Words::default();
    let mut geos = Words::default();
    let mut n_geos = 0u32;
    for (p, &(((tw, th), off), (ax, ay))) in run.planes.iter().zip(&sizes) {
        let matte = match &p.matte {
            Some(m) if m.len() == n_px => {
                let key = std::sync::Arc::as_ptr(m) as usize;
                let next = matte_ix.len() as u32 + 1;
                let ix = *matte_ix.entry(key).or_insert(next);
                if ix == next {
                    mattes.extend_from_slice(m);
                }
                ix
            }
            Some(_) => return None,
            None => 0,
        };
        planes.f([ax as f32, ay as f32, tw as f32, th as f32]);
        planes.d([off[0], off[1], p.buf.scale, if p.bicubic { 1.0 } else { 0.0 }]);
        planes.d(p.bbox.map(|v| v as f64));
        planes.d(p.bounds);
        affine_rows(&mut planes, &p.winv);
        planes.f([p.mat.ambient, p.mat.diffuse, p.mat.specular, p.mat.exponent() as f32]);
        planes.f([p.mat.metal, p.mat.light_transmission, p.opacity, 0.0]);
        let flags = p.draw as u32 | (p.preserve as u32) << 1 | (p.mat.accepts_lights as u32) << 2;
        planes.u([n_geos, p.geos.len() as u32, flags, mode_id(p.mode)]);
        planes.u([p.mat.casts_shadows, p.mat.accepts_shadows, p.layer_id, p.order as u32]);
        planes.u([matte, run.seed ^ p.layer_id, 0, 0]);
        for g in &p.geos {
            write_geo(&mut geos, g);
        }
        n_geos += p.geos.len() as u32;
    }
    if (mattes.len() * 4) as u64 > e.g.device.limits().max_storage_buffer_binding_size {
        return None;
    }
    let mut lights = Words::default();
    for l in &run.lights {
        write_light(&mut lights, l);
    }
    let mut casters = Words::default();
    for c in &run.casters {
        casters.0.extend_from_slice(&(*c as u32).to_le_bytes());
    }
    let pb = e.bytes(planes.at_least(PLANE_BYTES));
    let gb = e.bytes(geos.at_least(GEO_BYTES));
    let lb = e.bytes(lights.at_least(LIGHT_BYTES));
    let cb = e.bytes(casters.at_least(16));
    let mb = e.data(&mattes);
    let mut p = Params::default();
    p.u[0] = [run.planes.len() as u32, run.lights.len() as u32, run.casters.len() as u32, 0];
    let out = e.scratch(w, h);
    let groups = (w.div_ceil(16), h.div_ceil(16));
    match &atlas {
        Atlas::F32(atlas) => e.dispatch_ext("classic3d", &p, atlas, Some(canvas), &out, Some(&mb), groups, Some([&pb, &gb, &lb, &cb])),
        Atlas::Codes(atlas) => {
            let g = e.g;
            let (pipe, layout, lut) = g.codes.as_ref()?.classic3d_parts(&g.bgl_ext)?;
            e.dispatch_views("classic3d", pipe, layout, &p, [atlas.view(), &canvas.view, &out.view], Some(&mb), groups, Some([&pb, &gb, &lb, &cb]), Some(lut));
        }
    }
    Some(out)
}

#[cfg(test)]
thread_local! {
    /// Tests: this thread's runs use the f32 atlas (the codes atlas is compared against it).
    pub(crate) static F32_ATLAS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The plane atlas a run's kernel reads: f32, or 8 bpc codes (LVR1).
enum Atlas {
    F32(GpuImage),
    Codes(crate::codes::CodeImage),
}

/// LVR1: every plane of `run` as codes, without changing a value ([`crate::codes::exact_codes`]),
/// with its buffer offset; `None` (the f32 atlas) when one isn't on the 8 bpc grid, has depth of
/// field (blurred in f32), or the codes Classic 3D kernel isn't there.
fn codes_planes(e: &mut Enc, run: &Run3d, made: &HashMap<usize, GpuImage>) -> Option<Vec<(crate::codes::CodeImage, [f64; 2])>> {
    #[cfg(test)]
    if F32_ATLAS.with(std::cell::Cell::get) {
        return None;
    }
    if run.planes.iter().any(|p| p.dof.is_some()) || !e.g.codes.as_ref()?.has_classic3d(&e.g.bgl_ext) {
        return None;
    }
    let mut pixels = Vec::with_capacity(run.planes.len());
    for p in &run.planes {
        let px = match made.get(&(std::sync::Arc::as_ptr(&p.buf) as usize)) {
            Some(img) => crate::codes::Pixels::Float(img.clone()),
            None => e.upload_pixels(&p.buf)?,
        };
        // Checked for every plane before any is converted: a run that can't go as codes costs
        // nothing here.
        let exact = match &px {
            crate::codes::Pixels::Codes(_) => true,
            crate::codes::Pixels::Float(f) => f.levels == Some(crate::codes::LEVELS),
        };
        if !exact {
            return None;
        }
        pixels.push((px, p.buf.offset));
    }
    pixels.into_iter().map(|(px, off)| crate::codes::exact_codes(e, &px).map(|c| (c, off))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shelf_packing_fits_and_does_not_overlap() {
        let sizes = [(100, 40), (30, 90), (60, 60), (100, 10), (5, 5)];
        let (w, h, pos) = pack(&sizes, 256).unwrap();
        for (i, (&(x, y), &(sw, sh))) in pos.iter().zip(&sizes).enumerate() {
            assert!(x + sw <= w && y + sh <= h);
            for (j, (&(x2, y2), &(sw2, sh2))) in pos.iter().zip(&sizes).enumerate() {
                if i != j {
                    assert!(x + sw <= x2 || x2 + sw2 <= x || y + sh <= y2 || y2 + sh2 <= y, "{i} and {j} overlap");
                }
            }
        }
        assert!(pack(&[(300, 10)], 256).is_none());
        assert_eq!(max_overlap(&[[0, 0, 10, 10], [5, 5, 15, 15], [20, 20, 30, 30]]), 2);
    }
}
