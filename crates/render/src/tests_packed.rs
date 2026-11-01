//! Packed layer cache entries (`packed.rs`): lossless for quantised 8/16 bpc buffers, never
//! applied to anything else.

use std::sync::Arc;

use crate::packed::PackedBuf;
use crate::{Buf, LayerCache};
use effectcraft_raster::Image;

/// Deterministic premultiplied pixels (colour ≤ alpha, some transparent, some opaque).
fn noise(w: u32, h: u32, seed: u32) -> Image {
    let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        (s >> 8) as f32 / (1u32 << 24) as f32
    };
    let mut img = Image::new(w, h);
    for (i, p) in img.data.iter_mut().enumerate() {
        let a = match i % 7 {
            0 => 0.0,
            1 => 1.0,
            _ => next(),
        };
        *p = [next() * a, next() * a, next() * a, a];
    }
    img
}

fn buf(img: Image) -> Buf {
    Buf { img, offset: [-3.5, 12.25], scale: 0.5 }
}

/// Insert `b` as a buffer rendered a second time (admitted to the cache proper, not on
/// probation: see `cache.rs`).
fn keep(cache: &LayerCache, key: u64, b: Arc<Buf>) {
    cache.insert(key, b.clone());
    cache.insert(key, b);
}

fn bits(b: &Buf) -> Vec<[u32; 4]> {
    b.img.data.iter().map(|p| p.map(f32::to_bits)).collect()
}

#[test]
fn quantised_buffers_round_trip_bit_exactly() {
    for (levels, px_bytes) in [(255.0, 4), (32768.0, 8)] {
        let mut img = noise(37, 23, levels as u32);
        crate::color::quantize(&mut img, levels);
        let b = buf(img);
        let p = PackedBuf::pack(&b).unwrap_or_else(|| panic!("{levels} levels packs"));
        assert_eq!(p.bytes(), b.img.data.len() * px_bytes, "{levels} levels");
        let u = p.unpack();
        assert_eq!(bits(&u), bits(&b), "{levels} levels");
        assert_eq!((u.img.width, u.img.height, u.offset, u.scale), (b.img.width, b.img.height, b.offset, b.scale));
    }
}

#[test]
fn unquantised_buffers_are_not_packed() {
    // 32 bpc: off-grid values, over-range values, negative zero.
    assert!(PackedBuf::pack(&buf(noise(16, 16, 5))).is_none());
    let mut img = noise(4, 4, 6);
    crate::color::quantize(&mut img, 255.0);
    img.data[5] = [1.5, 1.0, 1.0, 1.0];
    assert!(PackedBuf::pack(&buf(img.clone())).is_none());
    img.data[5] = [-0.0, 0.0, 0.0, 0.0];
    assert!(PackedBuf::pack(&buf(img)).is_none());
}

/// With reserved output headroom, quantised entries pack losslessly. External
/// full-buffer pins remain charged even after their indexed entries are evicted.
#[test]
fn cache_packs_under_pressure_and_counts_packed_bytes() {
    let f32_bytes = 64 * 64 * 16;
    let cache = LayerCache::new(19 * f32_bytes / 4);
    let mut kept = vec![];
    for i in 0..6u64 {
        let mut img = noise(64, 64, i as u32);
        crate::color::quantize(&mut img, 255.0);
        let b = Arc::new(buf(img));
        kept.push(bits(&b));
        keep(&cache, i, b);
    }
    let st = cache.stats();
    assert_eq!(st.entries, 6, "nothing evicted");
    assert!(st.bytes < 6 * f32_bytes, "packing saved storage");
    assert_eq!(cache.retained_stats().live_bytes, st.bytes);
    assert!(cache.retained_stats().live_bytes <= cache.budget());
    // A hot unpack is retained when both the packed source and result fit.
    cache.set_budget(8 * f32_bytes);
    let back = cache.get(0).unwrap_or_else(|| panic!("entry 0"));
    assert_eq!(bits(&back), kept[0]);
    // A hit unpacks the entry for good: the next hit shares the same buffer.
    let again = cache.get(0).unwrap_or_else(|| panic!("entry 0"));
    assert!(Arc::ptr_eq(&back, &again));
    assert_eq!(cache.stats().bytes, st.bytes + 3 * f32_bytes / 4);
    for (i, want) in kept.iter().enumerate() {
        assert_eq!(&bits(&cache.get(i as u64).unwrap_or_else(|| panic!("entry {i}"))), want, "entry {i}");
    }

    // 32 bpc: never packed, the same Arc is served back.
    let cache = LayerCache::new(4 * f32_bytes);
    let bufs: Vec<_> = (0..6u64).map(|i| Arc::new(buf(noise(64, 64, 100 + i as u32)))).collect();
    for (i, b) in bufs.iter().enumerate() {
        keep(&cache, i as u64, b.clone());
    }
    let st = cache.stats();
    assert_eq!(st.bytes, st.entries * f32_bytes);
    assert!(st.entries < 6, "evicted, not packed");
    assert!(cache.get(5).is_none(), "externally pinned older allocations deny new retention");
    assert_eq!(cache.retained_stats().live_bytes, 4 * f32_bytes);
    drop(bufs);
    let fresh = Arc::new(buf(noise(64, 64, 200)));
    keep(&cache, 6, fresh.clone());
    assert!(Arc::ptr_eq(&cache.get(6).unwrap(), &fresh));
}

/// The renderer's own buffers: an 8 bpc layer's cached content packs and unpacks exactly.
#[test]
fn rendered_8bpc_layer_packs_losslessly() {
    let mut img = noise(40, 30, 77);
    crate::color::Pipe::of(&effectcraft_project::ProjectSettings::default()).quantize(&mut img);
    let b = buf(img);
    let p = PackedBuf::pack(&b).unwrap_or_else(|| panic!("default project (8 bpc) packs"));
    assert_eq!(p.bytes(), b.img.data.len() * 4);
    assert_eq!(bits(&p.unpack()), bits(&b));
}

/// Linear blending: a cached frame is served from the blending-space copies alone (one hit per
/// layer, not two), so the working-space buffers they came from go cold; pixels are unchanged.
#[test]
fn linear_blend_hits_skip_the_working_space_copy() {
    use effectcraft_color::Label;
    use effectcraft_project::{Comp, ItemKind, LayerSource, Project, Solid, build};
    use effectcraft_time::{FrameRate, Tick};

    let mut p = Project::default();
    p.settings.blend_linear = true;
    let comp = Comp::new(40, 20, FrameRate::FPS_30, Tick::from_seconds_f64(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    for (i, c) in [[0.8, 0.2, 0.1], [0.1, 0.5, 0.9]].into_iter().enumerate() {
        let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: c, width: 20 + i as u32 * 10, height: 20, pixel_aspect: 1.0 }));
        let mut l = build::layer(&mut p, &comp, "S", LayerSource::Solid { item: sid }, (20 + i as u32 * 10, 20), None);
        l.blend_mode = effectcraft_color::BlendMode::Screen;
        let Some(c) = p.comp_mut(cid) else { panic!("comp") };
        c.layers.insert(0, l);
    }
    let cache = LayerCache::default();
    let frame = || {
        let mut r = crate::Renderer::new(&p, &crate::NoFootage, crate::RenderOpts::default());
        r.cache = Some(&cache);
        r.comp_frame(cid, Tick::ZERO)
    };
    let a = frame();
    let entries = cache.stats().entries;
    assert_eq!(entries, 2, "two layers, each in the blending space alone (nothing reads the working space)");
    let hits = cache.stats().hits;
    let b = frame();
    assert_eq!(cache.stats().hits, hits + 2, "one hit per layer");
    assert_eq!(a.data, b.data);
    let uncached = crate::Renderer::new(&p, &crate::NoFootage, crate::RenderOpts::default()).comp_frame(cid, Tick::ZERO);
    assert_eq!(a.data, uncached.data);
}

#[test]
fn packed_output_reservation_covers_odd_sized_sixteen_bit_pixels() {
    let image = Image::filled(4099, 1, [1.0 / 32768.0, 0.0, 0.0, 1.0]);
    let b = buf(image);
    let p = PackedBuf::pack(&b).unwrap();
    assert_eq!(p.bytes(), 4099 * 8, "these pixels require the 16-bit attempt");
    assert_eq!(p.capacity_bytes(), Some(4099 * 8), "one requested output allocation");
    assert_eq!(bits(&p.unpack()), bits(&b));
}

#[test]
fn late_invalid_pixel_discards_partial_packed_output() {
    let mut image = Image::filled(4099, 1, [1.0 / 32768.0, 0.0, 0.0, 1.0]);
    image.data.last_mut().unwrap()[0] = -0.0;
    let b = buf(image);
    let original = bits(&b);
    assert!(PackedBuf::pack(&b).is_none());
    assert_eq!(bits(&b), original);
}

// The single round-and-pack kernels (1 Nov 2026) against the two passes they replaced: a copy
// of the old per-row quantise and the old per-value checked packing (`packed.rs` `pack_with`,
// the GPU's `pack_codes`), bit for bit.
mod against_two_pass {
    use crate::color::{PxRect, Region};
    use crate::packed::{codes8, codes16};
    use effectcraft_project::{BitDepth, ProjectSettings};
    use effectcraft_raster::Image;

    #[allow(clippy::manual_clamp)]
    fn old_quantize(img: &mut Image, r: PxRect, levels: f32) {
        let inv = 1.0 / levels;
        let w = img.width as usize;
        for y in r.y0 as usize..r.y1 as usize {
            for p in &mut img.data[y * w + r.x0 as usize..y * w + r.x1 as usize] {
                let a = ((p[3].max(0.0).min(1.0) * levels + 0.5) as u32 as f32) * inv;
                for c in 0..3 {
                    p[c] = ((p[c].max(0.0).min(a) * levels + 0.5) as u32 as f32) * inv;
                }
                p[3] = a;
            }
        }
    }

    fn old_pack<T: Copy>(data: &[[f32; 4]], levels: f32, conv: impl Fn(u32) -> T) -> Option<Vec<[T; 4]>> {
        let inv = 1.0 / levels;
        let level = |v: f32| -> Option<T> {
            if !(0.0..=1.0).contains(&v) {
                return None;
            }
            let k = (v * levels + 0.5) as u32;
            ((k as f32 * inv).to_bits() == v.to_bits()).then(|| conv(k))
        };
        data.iter().map(|p| Some([level(p[0])?, level(p[1])?, level(p[2])?, level(p[3])?])).collect()
    }

    fn bits(img: &Image) -> Vec<[u32; 4]> {
        img.data.iter().map(|p| p.map(f32::to_bits)).collect()
    }

    /// Values that sit on, next to and far off both grids.
    fn specials() -> Vec<f32> {
        let mut v = vec![
            0.0,
            -0.0,
            1.0,
            -1.0,
            2.0,
            1.5,
            f32::NAN,
            -f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            -f32::from_bits(1),
            f32::MAX,
            f32::MIN,
            0.5 / 255.0,
            0.5 / 32768.0,
            1.0 + f32::EPSILON,
            1.0 - f32::EPSILON / 2.0,
            u32::MAX as f32,
        ];
        for levels in [255.0f32, 32768.0] {
            for k in [0u32, 1, 2, 127, 128, 254, 255, 256, 16_384, 32_767, 32_768] {
                let x = k as f32 * (1.0 / levels);
                v.extend([x, f32::from_bits(x.to_bits() + 1), f32::from_bits(x.to_bits().saturating_sub(1)), (k as f32 + 0.5) / levels]);
            }
        }
        v
    }

    /// Premultiplied pixels with every special in every lane, colour above alpha, and noise.
    fn image(w: u32, h: u32, seed: u32) -> Image {
        let sp = specials();
        let mut s = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s
        };
        let mut img = Image::new(w, h);
        for (i, p) in img.data.iter_mut().enumerate() {
            let r = next();
            *p = match r % 4 {
                0 => std::array::from_fn(|_| sp[next() as usize % sp.len()]),
                1 => {
                    let mut unit = || (next() >> 8) as f32 / (1u32 << 24) as f32;
                    let a = unit();
                    // Colour up to 1.3 × alpha (over alpha: clamped to it).
                    [unit() * a * 1.3, unit() * a * 1.3, unit() * a * 1.3, a]
                }
                2 => [sp[i % sp.len()], sp[(i / 3) % sp.len()], sp[(i / 7) % sp.len()], sp[(i / 11) % sp.len()]],
                _ => [-0.25, 0.75, 1.75, 0.5],
            };
        }
        img
    }

    fn pipe(depth: BitDepth) -> crate::color::Pipe {
        crate::color::Pipe::of(&ProjectSettings { bit_depth: depth, ..Default::default() })
    }

    #[test]
    fn quantize_matches_the_old_pass_on_every_depth_size_and_region() {
        let sizes = [(1, 1), (1, 7), (7, 1), (3, 5), (17, 13), (255, 3), (1921, 3), (4097, 2), (300, 260)];
        for (si, &(w, h)) in sizes.iter().enumerate() {
            let src = image(w, h, si as u32 + 1);
            let full = PxRect { x0: 0, y0: 0, x1: w, y1: h };
            let rects =
                [full, PxRect { x0: w / 3, y0: h / 2, x1: w - w / 4, y1: h }, PxRect { x0: 0, y0: 0, x1: 1, y1: 1 }, PxRect { x0: w - 1, y0: 0, x1: w, y1: h }];
            for (depth, levels) in [(BitDepth::Bpc8, Some(255.0)), (BitDepth::Bpc16, Some(32768.0)), (BitDepth::Bpc32, None)] {
                let p = pipe(depth);
                for r in rects {
                    let mut new = src.clone();
                    // Pixels outside the region must already be quantised (the debug check).
                    if let Some(l) = levels {
                        old_quantize(&mut new, full, l);
                    }
                    let mut old = new.clone();
                    if let Some(l) = levels {
                        old_quantize(&mut old, r, l);
                    }
                    p.quantize_region(&mut new, Region::Rect(r));
                    assert_eq!(bits(&new), bits(&old), "{w}×{h} {depth:?} {r:?}");
                }
                let mut old = src.clone();
                if let Some(l) = levels {
                    old_quantize(&mut old, full, l);
                }
                let mut new = src.clone();
                p.quantize(&mut new);
                assert_eq!(bits(&new), bits(&old), "{w}×{h} {depth:?} full");
                // `quantize_px` (one pixel) agrees too.
                for (i, px) in src.data.iter().enumerate().take(64) {
                    assert_eq!(p.quantize_px(*px).map(f32::to_bits), old.data[i].map(f32::to_bits), "{w}×{h} {depth:?} pixel {i}");
                }
            }
        }
    }

    #[test]
    fn codes_match_the_old_packing_on_and_off_the_grid() {
        let sizes = [(1, 1), (3, 5), (17, 13), (1921, 3), (300, 260), (65_537, 1)];
        for (si, &(w, h)) in sizes.iter().enumerate() {
            let raw = image(w, h, 100 + si as u32);
            for levels in [255.0f32, 32768.0] {
                let mut q = raw.clone();
                old_quantize(&mut q, PxRect { x0: 0, y0: 0, x1: w, y1: h }, levels);
                for img in [&q, &raw] {
                    assert_eq!(codes8(&img.data), old_pack(&img.data, 255.0, |k| k as u8), "{w}×{h} {levels} 8");
                    assert_eq!(codes16(&img.data), old_pack(&img.data, 32768.0, |k| k as u16), "{w}×{h} {levels} 16");
                }
                let want = if levels == 255.0 { codes8(&q.data).map(|c| c.len()) } else { codes16(&q.data).map(|c| c.len()) };
                assert_eq!(want, Some(q.data.len()), "{w}×{h}: a quantised buffer packs on its own grid");
            }
            // One bad value anywhere (first, middle, last band) declines, as before.
            let mut q = raw.clone();
            old_quantize(&mut q, PxRect { x0: 0, y0: 0, x1: w, y1: h }, 255.0);
            let n = q.data.len();
            for (at, bad) in [(0, f32::NAN), (n / 2, -0.0), (n - 1, 1.0 + f32::EPSILON), (n - 1, f32::INFINITY), (n / 3, -f32::from_bits(1))] {
                let mut b = q.clone();
                b.data[at][at % 4] = bad;
                assert_eq!(codes8(&b.data), None, "{w}×{h} {bad} at {at}");
                assert_eq!(old_pack(&b.data, 255.0, |k| k as u8), None);
            }
        }
    }

    /// Every code of both grids, and both neighbouring floats of each, alone in a buffer.
    #[test]
    fn every_grid_value_and_its_neighbours_pack_as_before() {
        for levels in [255.0f32, 32768.0] {
            let inv = 1.0 / levels;
            let mut data = Vec::new();
            for k in 0..=levels as u32 {
                let x = k as f32 * inv;
                data.push([x, x, x, x]);
                for n in [f32::from_bits(x.to_bits() + 1), f32::from_bits(x.to_bits().wrapping_sub(1))] {
                    let one = [[n, 0.0, 0.0, 0.0]];
                    assert_eq!(codes8(&one), old_pack(&one, 255.0, |k| k as u8), "{levels} {n}");
                    assert_eq!(codes16(&one), old_pack(&one, 32768.0, |k| k as u16), "{levels} {n}");
                }
            }
            assert_eq!(codes8(&data), old_pack(&data, 255.0, |k| k as u8), "{levels}");
            assert_eq!(codes16(&data), old_pack(&data, 32768.0, |k| k as u16), "{levels}");
            assert!(if levels == 255.0 { codes8(&data).is_some() } else { codes16(&data).is_some() });
        }
        // Each special alone among valid values: one past the grid's top (256/255, which
        // round-trips through its code) still declines.
        for v in specials().into_iter().chain([256.0 / 255.0, 32_769.0 / 32_768.0, 510.0 / 255.0]) {
            for one in [[[v, 0.0, 0.0, 1.0]], [[0.0, 0.0, 0.0, v]]] {
                assert_eq!(codes8(&one), old_pack(&one, 255.0, |k| k as u8), "{v}");
                assert_eq!(codes16(&one), old_pack(&one, 32768.0, |k| k as u16), "{v}");
            }
        }
    }

    /// The single round-and-pack pass (`color::quantize8_codes`, 1 Nov 2026) is the two passes
    /// it replaces on the GPU walk's resident sources: the old quantise then the old packing,
    /// bit for bit (the image and its codes), on 0, 1, over 1, negatives, NaN, infinities,
    /// subnormals, colour over alpha, noise, every (colour, alpha) code pair with the halfway
    /// values between them, and band edges (65 536 pixels a band).
    #[test]
    fn quantize8_codes_is_the_two_passes() {
        let check = |src: &Image, what: &str| {
            let full = PxRect { x0: 0, y0: 0, x1: src.width, y1: src.height };
            let mut old = src.clone();
            old_quantize(&mut old, full, 255.0);
            let want = old_pack(&old.data, 255.0, |k| k as u8).unwrap_or_else(|| panic!("{what}: a quantised image packs"));
            let mut new = src.clone();
            let codes = crate::color::quantize8_codes(&mut new).unwrap_or_else(|| panic!("{what}: codes"));
            assert_eq!(bits(&new), bits(&old), "{what}: pixels");
            assert!(codes == want, "{what}: codes");
            assert_eq!(codes8(&new.data).as_deref(), Some(&codes[..]), "{what}: what the upload would have packed");
            // And the image is what the plain 8 bpc pass leaves.
            let mut plain = src.clone();
            pipe(BitDepth::Bpc8).quantize(&mut plain);
            assert_eq!(bits(&plain), bits(&new), "{what}: pipe quantise");
        };
        let sizes = [(1, 1), (3, 5), (17, 13), (1921, 3), (300, 260), (65_535, 1), (65_536, 1), (65_537, 1), (256, 513)];
        for (si, &(w, h)) in sizes.iter().enumerate() {
            check(&image(w, h, 300 + si as u32), &format!("{w}×{h}"));
        }
        // Every alpha code with every colour code at and between the grid's values.
        let mut data = Vec::new();
        for ka in 0..=255u32 {
            let a = ka as f32 / 255.0;
            for kc in 0..=255u32 {
                let c = kc as f32 / 255.0;
                data.push([c, (kc as f32 + 0.5) / 255.0, (kc as f32 - 0.5) / 255.0, a]);
                data.push([c * a, c, 1.0 - c, (ka as f32 + 0.5) / 255.0]);
            }
        }
        let n = data.len() as u32;
        check(&Image { width: n, height: 1, data: data.clone() }, "code pairs");
        check(&Image { width: 512, height: n / 512, data }, "code pairs 2D");
        // Specials in every lane.
        let sp = specials();
        let data: Vec<[f32; 4]> = sp.iter().flat_map(|&v| [[v, 0.5, 0.25, 1.0], [0.25, v, v, 0.75], [0.0, 0.0, 0.0, v], [v, v, v, v]]).collect();
        check(&Image { width: data.len() as u32, height: 1, data }, "specials");
    }
}

/// The resident source the GPU walk uploads (`Renderer::resident_source_coded`): at 8 bpc its
/// codes come out of the rounding pass and are exactly what the upload packed before
/// (`codes8` of the buffer); 16 and 32 bpc make no codes. Every depth gives the same buffer as
/// `resident_source`, bit for bit.
#[test]
fn resident_source_codes_at_8bpc_only_same_pixels_every_depth() {
    use effectcraft_color::Label;
    use effectcraft_project::build::{self, Ids};
    use effectcraft_project::{BitDepth, Comp, ItemKind, LayerSource, Project};
    use effectcraft_time::{FrameRate, Tick};

    for depth in [BitDepth::Bpc8, BitDepth::Bpc16, BitDepth::Bpc32] {
        let mut p = Project::default();
        p.settings.bit_depth = depth;
        let comp = Comp::new(64, 48, FrameRate::FPS_30, Tick::from_seconds_f64(1.0));
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
        let mut l = build::layer(&mut p, &comp, "Shape", LayerSource::Shape, (64, 48), None);
        let mut next = p.next_id;
        let mut ids = Ids(&mut next);
        // Anti-aliased edges and an off-grid colour: real rounding to do.
        let ellipse = build::shape_ellipse(&mut ids, [37.0, 29.0], [0.0, 0.0]);
        let fill = build::shape_fill(&mut ids, [0.9, 0.4, 0.13, 0.7]);
        let g = build::shape_group(&mut ids, "Ellipse", vec![ellipse, fill]);
        p.next_id = next;
        l.props.sub_mut("contents").unwrap().children.push(g.into());
        p.comp_mut(cid).unwrap().layers.push(l);
        let r = crate::Renderer::new(&p, &crate::NoFootage, crate::RenderOpts::default());
        let ctx = r.eval_ctx(cid, Tick::ZERO).unwrap();
        let layer = &p.comp(cid).unwrap().layers[0];
        let clip = crate::ContentClip::default();
        let plain = r.resident_source(&ctx, layer, clip).unwrap();
        let coded = r.resident_source_coded(&ctx, layer, clip, true).unwrap();
        let b = coded.buf();
        assert_eq!(bits(b), bits(&plain), "{depth:?}");
        assert_eq!((b.img.width, b.img.height, b.offset, b.scale), (plain.img.width, plain.img.height, plain.offset, plain.scale), "{depth:?}");
        assert!(plain.img.data.iter().any(|p| p[3] > 0.0 && p[3] < 1.0), "{depth:?}: has edges");
        match depth {
            BitDepth::Bpc8 => {
                let want = crate::packed::codes8(&plain.img.data).unwrap();
                assert!(coded.codes8() == Some(&want[..]), "8 bpc: the codes the upload packed");
                assert_eq!(coded.bytes(), plain.img.data.len() * 20);
            }
            _ => assert!(coded.codes8().is_none(), "{depth:?}: no codes"),
        }
        assert!(r.resident_source_coded(&ctx, layer, clip, false).unwrap().codes8().is_none(), "{depth:?}: codes only when asked");
    }
}
