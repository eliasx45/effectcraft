//! Native integer video planes to premultiplied RGBA, with no CPU expansion.
use crate::context::{Enc, GpuContext, GpuImage, buffer_init, init_resource};
use effectcraft_render::yuv::{Plane, Samples, View};

/// Exact descriptor identity, in addition to the decoded allocation owner and
/// renderer dependency key. No pixel hashing or lossy descriptor hash is needed.
pub(crate) fn metadata(view: View<'_>) -> Vec<u64> {
    let mut key = vec![
        u64::from(view.width),
        u64::from(view.height),
        u64::from(view.bits),
        view.siting as u64,
        u64::from(view.subsampling[0]),
        u64::from(view.subsampling[1]),
        view.alpha_mode as u64,
        view.alpha.is_some() as u64,
    ];
    for p in view.planes.iter().chain(view.alpha.iter()) {
        let pointer = match p.samples {
            Samples::U8(v) => v.as_ptr() as usize,
            Samples::U16(v) => v.as_ptr() as usize,
        };
        key.extend([pointer as u64, p.samples.len() as u64, p.samples.bytes_per_sample() as u64, p.stride as u64, u64::from(p.width), u64::from(p.height)]);
    }
    key.extend(view.coefficients.luma.iter().chain(&view.coefficients.chroma).chain(&view.coefficients.rgb).chain(&view.matte).map(|v| u64::from(v.to_bits())));
    key
}

pub(crate) struct Pipelines {
    decode: wgpu::ComputePipeline,
    decode_layout: wgpu::BindGroupLayout,
    mask: wgpu::ComputePipeline,
    mask_layout: wgpu::BindGroupLayout,
    /// Packed layer codes to floats (`unpack.wgsl`, [`GpuContext::upload_image`]).
    pub(crate) unpack: wgpu::ComputePipeline,
    pub(crate) unpack_layout: wgpu::BindGroupLayout,
    /// Retained 8 bpc images to codes (`pack.wgsl`, [`crate::context::PackedImage`]); `None`
    /// where `Rgba8Uint` can't be a storage texture.
    pub(crate) pack: Option<(wgpu::ComputePipeline, wgpu::BindGroupLayout)>,
}

impl Pipelines {
    pub(crate) fn new(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Result<Self, String> {
        let usage = wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING;
        for format in [
            wgpu::TextureFormat::R8Uint,
            wgpu::TextureFormat::R16Uint,
            wgpu::TextureFormat::R32Float,
            wgpu::TextureFormat::Rgba8Uint,
            wgpu::TextureFormat::Rgba16Uint,
        ] {
            if !adapter.get_texture_format_features(format).allowed_usages.contains(usage) {
                return Err(format!("native video plane format {format:?} lacks upload/sampling support"));
            }
        }
        let module = init_resource(device, "YUV shader", || {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("native YUV"),
                source: wgpu::ShaderSource::Wgsl(
                    format!(
                        "{}\n{}\n{}\n{}",
                        include_str!("ieee_add.wgsl"),
                        include_str!("ieee_mul.wgsl"),
                        include_str!("yuv.wgsl"),
                        include_str!("unpack.wgsl")
                    )
                    .into(),
                ),
            })
        })?;
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let texture = |sample_type| wgpu::BindingType::Texture { sample_type, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false };
        let output = wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba32Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        };
        let decode_layout = init_resource(device, "YUV decode layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("YUV decode"),
                entries: &[
                    entry(
                        0,
                        wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(64),
                        },
                    ),
                    entry(1, texture(wgpu::TextureSampleType::Uint)),
                    entry(2, texture(wgpu::TextureSampleType::Uint)),
                    entry(3, texture(wgpu::TextureSampleType::Uint)),
                    entry(4, texture(wgpu::TextureSampleType::Uint)),
                    entry(5, output),
                ],
            })
        })?;
        let mask_layout = init_resource(device, "YUV mask layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("YUV mask"),
                entries: &[
                    entry(5, output),
                    entry(6, texture(wgpu::TextureSampleType::Float { filterable: false })),
                    entry(7, texture(wgpu::TextureSampleType::Float { filterable: false })),
                ],
            })
        })?;
        let unpack_layout = init_resource(device, "packed upload layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("packed upload"),
                entries: &[
                    entry(5, output),
                    entry(8, texture(wgpu::TextureSampleType::Uint)),
                    entry(
                        9,
                        wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(16),
                        },
                    ),
                ],
            })
        })?;
        let pipeline = |name, bgl: &wgpu::BindGroupLayout| {
            let layout = init_resource(device, "YUV pipeline layout", || {
                device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(name), bind_group_layouts: &[Some(bgl)], immediate_size: 0 })
            })?;
            init_resource(device, "YUV pipeline", || {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some(name),
                    layout: Some(&layout),
                    module: &module,
                    entry_point: Some(name),
                    compilation_options: Default::default(),
                    cache: None,
                })
            })
        };
        let decode = pipeline("decode", &decode_layout)?;
        let mask = pipeline("mask", &mask_layout)?;
        let unpack = pipeline("unpack", &unpack_layout)?;
        let pack = Self::pack(adapter, device).map_err(|e| log::info!("gpu: retained images stay 32-bit float ({e})")).ok();
        Ok(Self { decode, decode_layout, mask, mask_layout, unpack, unpack_layout, pack })
    }

    fn pack(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Result<(wgpu::ComputePipeline, wgpu::BindGroupLayout), String> {
        let format = wgpu::TextureFormat::Rgba8Uint;
        let usage = wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
        if !adapter.get_texture_format_features(format).allowed_usages.contains(usage) {
            return Err("no Rgba8Uint storage textures".into());
        }
        let module = init_resource(device, "pack shader", || {
            device
                .create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("pack"), source: wgpu::ShaderSource::Wgsl(include_str!("pack.wgsl").into()) })
        })?;
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::COMPUTE, ty, count: None };
        let layout = init_resource(device, "pack layout", || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("pack"),
                entries: &[
                    entry(
                        0,
                        wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                    ),
                    entry(
                        1,
                        wgpu::BindingType::StorageTexture {
                            access: wgpu::StorageTextureAccess::WriteOnly,
                            format,
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                    ),
                ],
            })
        })?;
        let pipeline_layout = init_resource(device, "pack pipeline layout", || {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("pack8"), bind_group_layouts: &[Some(&layout)], immediate_size: 0 })
        })?;
        let pipeline = init_resource(device, "pack pipeline", || {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("pack8"),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some("pack8"),
                compilation_options: Default::default(),
                cache: None,
            })
        })?;
        Ok((pipeline, layout))
    }
}

fn upload(g: &GpuContext, plane: Plane<'_>) -> Option<wgpu::Texture> {
    let n = plane.required_bytes()?;
    if !g.fits(plane.width, plane.height) {
        return None;
    }
    let (format, bytes): (_, &[u8]) = match plane.samples {
        Samples::U8(v) => (wgpu::TextureFormat::R8Uint, v),
        Samples::U16(v) if cfg!(target_endian = "little") => (wgpu::TextureFormat::R16Uint, bytemuck::cast_slice(v)),
        _ => return None,
    };
    let stride = u32::try_from(plane.stride.checked_mul(plane.samples.bytes_per_sample())?).ok()?;
    let texture = g.plane_texture(plane.width, plane.height, format, "native video plane");
    g.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        bytes.get(..n)?,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: Some(plane.height) },
        texture.size(),
    );
    g.transfers.count(true, n);
    Some(texture)
}

pub(crate) fn decode(e: &mut Enc, view: View<'_>) -> Option<GpuImage> {
    let _profile = effectcraft_render::profile::span("yuv_upload");
    let input_bytes = view.validate().ok()?;
    let g = e.g;
    let p = g.yuv.as_ref()?;
    if !g.fits(view.width, view.height) {
        return None;
    }
    // HWDEC2: the hardware decoder's own surface when the platform can share it, else an upload.
    let surface = crate::surface::planes(e, view);
    let from_surface = surface.is_some();
    let [y, u, v] = match surface {
        Some(planes) => planes,
        None => [upload(g, view.planes[0])?, upload(g, view.planes[1])?, upload(g, view.planes[2])?],
    };
    let a = match view.alpha {
        Some(a) => Some(upload(g, a)?),
        None => None,
    };
    let views = [&y, &u, &v, a.as_ref().unwrap_or(&y)].map(|t| t.create_view(&Default::default()));
    let mode = match view.alpha_mode {
        effectcraft_project::AlphaMode::Ignore => 0,
        effectcraft_project::AlphaMode::Straight => 1,
        effectcraft_project::AlphaMode::Premultiplied => 2,
    };
    let header = [view.subsampling[0], view.subsampling[1], u32::from(view.alpha.is_some()), mode];
    let values = [
        view.coefficients.luma[0],
        view.coefficients.luma[1],
        view.coefficients.chroma[0],
        view.coefficients.chroma[1],
        view.coefficients.rgb[0],
        view.coefficients.rgb[1],
        view.coefficients.rgb[2],
        view.coefficients.rgb[3],
        view.matte[0],
        view.matte[1],
        view.matte[2],
        1.0 / ((1u32 << view.bits) - 1) as f32,
    ];
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(bytemuck::cast_slice(&header));
    bytes.extend_from_slice(bytemuck::cast_slice(&values));
    let uniform = buffer_init(&g.device, &g.queue, Some("YUV coefficients"), &bytes, wgpu::BufferUsages::UNIFORM);
    let out = e.scratch(view.width, view.height);
    let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("native YUV"),
        layout: &p.decode_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[0]) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[1]) },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&views[2]) },
            wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(&views[3]) },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&out.view) },
        ],
    });
    e.custom_pixels("yuv_decode", &p.decode, &bg, (view.width, view.height));
    // (surface planes are not upload planes: they don't go back to the upload pool)
    if !from_surface {
        for t in [Some(y), Some(u), Some(v), a].into_iter().flatten() {
            e.stage(t);
        }
        g.transfers.count_yuv(input_bytes);
    }
    Some(out)
}

pub(crate) fn mask(e: &mut Enc, source: &GpuImage, mask: &effectcraft_raster::Mask) -> Option<GpuImage> {
    let g = e.g;
    let p = g.yuv.as_ref()?;
    if (source.width, source.height) != (mask.width, mask.height) {
        return None;
    }
    let n = (mask.width as usize).checked_mul(mask.height as usize)?;
    let data = mask.data.get(..n)?;
    let bytes = bytemuck::cast_slice(data);
    let texture = g.plane_texture(mask.width, mask.height, wgpu::TextureFormat::R32Float, "source mask plane");
    g.queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        bytes,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(mask.width.checked_mul(4)?), rows_per_image: Some(mask.height) },
        texture.size(),
    );
    g.transfers.count(true, bytes.len());
    let view = texture.create_view(&Default::default());
    let out = e.scratch(mask.width, mask.height);
    let bg = g.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("source mask"),
        layout: &p.mask_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::TextureView(&out.view) },
            wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&source.view) },
            wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::TextureView(&view) },
        ],
    });
    e.custom_pixels("yuv_source_mask", &p.mask, &bg, (mask.width, mask.height));
    e.stage(texture);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use effectcraft_project::AlphaMode;
    use effectcraft_render::yuv::{ChromaSiting, Coefficients};

    fn sample(plane: Plane<'_>, x: usize, y: usize) -> f32 {
        let i = y * plane.stride + x;
        match plane.samples {
            Samples::U8(v) => v[i] as f32,
            Samples::U16(v) => v[i] as f32,
        }
    }
    fn oracle(v: View<'_>) -> effectcraft_raster::Image {
        let mut img = effectcraft_raster::Image::new(v.width, v.height);
        for y in 0..v.height as usize {
            for x in 0..v.width as usize {
                let c = |plane: Plane<'_>| {
                    let cx = (x >> v.subsampling[0]).min(plane.width as usize - 1);
                    let cy = (y >> v.subsampling[1]).min(plane.height as usize - 1);
                    let ox = if x % 2 == 0 { cx.saturating_sub(1) } else { (cx + 1).min(plane.width as usize - 1) };
                    let oy = if y % 2 == 0 { cy.saturating_sub(1) } else { (cy + 1).min(plane.height as usize - 1) };
                    let vertical = |xx| {
                        let raw = if v.subsampling[1] == 0 { sample(plane, xx, cy) } else { sample(plane, xx, cy) * 0.75 + sample(plane, xx, oy) * 0.25 };
                        (raw - v.coefficients.chroma[0]) * v.coefficients.chroma[1]
                    };
                    if v.subsampling[0] == 0 { vertical(cx) } else { vertical(cx) * 0.75 + vertical(ox) * 0.25 }
                };
                let yy = (sample(v.planes[0], x, y) - v.coefficients.luma[0]) * v.coefficients.luma[1];
                let (u, vv) = (c(v.planes[1]), c(v.planes[2]));
                let k = v.coefficients.rgb;
                let mut rgb = [yy + k[0] * vv, yy - k[1] * vv - k[2] * u, yy + k[3] * u].map(|v| v.clamp(0.0, 1.0));
                let a = if v.alpha_mode == AlphaMode::Ignore {
                    1.0
                } else {
                    v.alpha.map_or(1.0, |p| (sample(p, x, y) / ((1u32 << v.bits) - 1) as f32).clamp(0.0, 1.0))
                };
                match v.alpha_mode {
                    AlphaMode::Straight => rgb.iter_mut().for_each(|v| *v *= a),
                    AlphaMode::Premultiplied => rgb.iter_mut().zip(v.matte).for_each(|(v, m)| *v = (*v - m * (1.0 - a)).max(0.0)),
                    AlphaMode::Ignore => (),
                }
                img.data[y * v.width as usize + x] = [rgb[0], rgb[1], rgb[2], a];
            }
        }
        img
    }

    #[test]
    fn native_yuv_layouts_ranges_alpha_and_edges_match_cpu() {
        let Some(g) = crate::tests::gpu() else { return };
        assert!(g.ctx.yuv.is_some(), "{:?}", g.ctx.unavailable_kernels());
        for (w, h) in [(1u32, 1u32), (3, 7), (34, 19)] {
            for shifts in [[0, 0], [1, 0], [1, 1]] {
                for bits in [8u32, 10, 12, 16] {
                    let cw = w.div_ceil(1 << shifts[0]);
                    let ch = h.div_ceil(1 << shifts[1]);
                    // Padded strides deliberately exercise plane upload validation.
                    let shapes = [(w, h), (cw, ch), (cw, ch), (w, h)];
                    let data: [Vec<u16>; 4] = std::array::from_fn(|i| {
                        (0..((shapes[i].0 + 2) * shapes[i].1))
                            .map(|j| (j.wrapping_mul(2654435761).wrapping_add(i as u32 * 997) & ((1 << bits) - 1)) as u16)
                            .collect()
                    });
                    let bytes: [Vec<u8>; 4] = std::array::from_fn(|i| data[i].iter().map(|v| *v as u8).collect());
                    let planes: [Plane; 4] = std::array::from_fn(|i| Plane {
                        samples: if bits == 8 { Samples::U8(&bytes[i]) } else { Samples::U16(&data[i]) },
                        width: shapes[i].0,
                        height: shapes[i].1,
                        stride: shapes[i].0 as usize + 2,
                    });
                    for full in [false, true] {
                        for mode in [AlphaMode::Straight, AlphaMode::Premultiplied, AlphaMode::Ignore] {
                            let s = (1u32 << (bits - 8)) as f32;
                            let max = ((1u32 << bits) - 1) as f32;
                            let v = View {
                                width: w,
                                height: h,
                                planes: [planes[0], planes[1], planes[2]],
                                alpha: Some(planes[3]),
                                bits,
                                subsampling: shifts,
                                siting: ChromaSiting::Centered,
                                coefficients: Coefficients {
                                    luma: if full { [0.0, 1.0 / max] } else { [16.0 * s, 1.0 / (219.0 * s)] },
                                    chroma: [128.0 * s, if full { 1.0 / max } else { 1.0 / (224.0 * s) }],
                                    rgb: [1.5748, 0.46812427, 0.18732427, 1.8556],
                                },
                                alpha_mode: mode,
                                matte: [0.7, 0.2, 0.5],
                            };
                            let mut e = Enc::new(&g.ctx);
                            let decoded = decode(&mut e, v).expect("native planes accepted");
                            let got = e.download(&decoded).expect("readback");
                            let want = oracle(v);
                            for (i, (a, b)) in got.data.iter().zip(&want.data).enumerate() {
                                assert!(
                                    a.iter().zip(b).all(|(a, b)| a.is_finite() && (a - b).abs() <= 1e-3),
                                    "{w}x{h} {shifts:?} {bits} {mode:?} full={full} pixel{i}: {a:?} != {b:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[derive(Clone)]
    struct NativeFixture {
        planes: [Vec<u8>; 3],
        native: bool,
    }
    impl NativeFixture {
        fn new() -> Self {
            Self {
                native: true,
                planes: std::array::from_fn(|i| (0..if i == 0 { 97 * 61 } else { 49 * 31 }).map(|j| ((j * 13 + i * 71) % 220 + 16) as u8).collect()),
            }
        }
        fn view(&self) -> View<'_> {
            View {
                width: 97,
                height: 61,
                planes: std::array::from_fn(|i| Plane {
                    samples: Samples::U8(&self.planes[i]),
                    width: if i == 0 { 97 } else { 49 },
                    height: if i == 0 { 61 } else { 31 },
                    stride: if i == 0 { 97 } else { 49 },
                }),
                alpha: None,
                bits: 8,
                subsampling: [1, 1],
                siting: ChromaSiting::Centered,
                coefficients: Coefficients { luma: [16.0, 1.0 / 219.0], chroma: [128.0, 1.0 / 224.0], rgb: [1.5748, 0.46812427, 0.18732427, 1.8556] },
                alpha_mode: AlphaMode::Straight,
                matte: [0.0; 3],
            }
        }
    }
    impl effectcraft_render::FramePixels for NativeFixture {
        fn size(&self) -> (u32, u32) {
            (97, 61)
        }
        fn rows(&self, y0: usize, out: &mut [effectcraft_raster::Px]) {
            let image = oracle(self.view());
            out.copy_from_slice(&image.data[y0 * 97..y0 * 97 + out.len()]);
        }
        fn yuv(&self) -> Option<View<'_>> {
            self.native.then(|| self.view())
        }
    }
    impl effectcraft_render::FootageSource for NativeFixture {
        fn frame(
            &self,
            _: effectcraft_project::ItemId,
            _: &effectcraft_project::Footage,
            _: effectcraft_time::Tick,
        ) -> Option<std::sync::Arc<effectcraft_raster::Image>> {
            Some(std::sync::Arc::new(oracle(self.view())))
        }
        fn frame_pixels(
            &self,
            _: effectcraft_project::ItemId,
            _: &effectcraft_project::Footage,
            _: effectcraft_time::Tick,
        ) -> Option<effectcraft_render::SharedFrame> {
            Some(std::sync::Arc::new(self.clone()))
        }
    }

    struct StableNative(std::sync::Arc<NativeFixture>);
    impl effectcraft_render::FootageSource for StableNative {
        fn frame(
            &self,
            _: effectcraft_project::ItemId,
            _: &effectcraft_project::Footage,
            _: effectcraft_time::Tick,
        ) -> Option<std::sync::Arc<effectcraft_raster::Image>> {
            Some(std::sync::Arc::new(oracle(self.0.view())))
        }
        fn frame_pixels(
            &self,
            _: effectcraft_project::ItemId,
            _: &effectcraft_project::Footage,
            _: effectcraft_time::Tick,
        ) -> Option<effectcraft_render::SharedFrame> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn native_yuv_cache_guards_owner_metadata_clear_and_publication() {
        use crate::context::NativeImage;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let Some(g) = crate::tests::gpu() else { return };
        g.ctx.clear_uploads();
        g.ctx.set_cache_budget(4096);
        let cache = effectcraft_render::LayerCache::new(4096);
        let owner: effectcraft_render::SharedFrame = Arc::new(NativeFixture::new());
        let other: effectcraft_render::SharedFrame = Arc::new(NativeFixture::new());
        let token = cache.resident_token().unwrap();
        let pixels = effectcraft_raster::Image::new(8, 8);
        let image = NativeImage { img: g.ctx.upload_image(&pixels).unwrap(), offset: [0.0; 2], scale: 1.0 };
        let mut e = Enc::with_owner(&g.ctx, cache.gpu_owner());
        e.keep_native(owner.clone(), 7, token.clone(), vec![1], image.clone());
        assert!(g.ctx.native_get(&owner, 7, &token, &[1], cache.gpu_owner().as_ref()).is_none(), "unsubmitted encoder cannot publish");
        cache.clear();
        e.publish_native();
        assert_eq!(g.ctx.upload_stats(), (0, 0), "late publication after clear is rejected");
        assert!(!token.valid());
        let token = cache.resident_token().unwrap();
        let mut e = Enc::with_owner(&g.ctx, cache.gpu_owner());
        e.keep_native(owner.clone(), 7, token.clone(), vec![1], image.clone());
        e.publish_native();
        assert_eq!(g.ctx.upload_stats(), (1, 1024));
        assert!(g.ctx.native_get(&owner, 7, &token, &[1], cache.gpu_owner().as_ref()).is_some());
        assert!(g.ctx.native_get(&other, 7, &token, &[1], cache.gpu_owner().as_ref()).is_none());
        assert!(g.ctx.native_get(&owner, 7, &token, &[2], cache.gpu_owner().as_ref()).is_none());
        let mut pending = Enc::with_owner(&g.ctx, cache.gpu_owner());
        pending.keep_native(owner.clone(), 9, token.clone(), vec![1], image.clone());
        g.ctx.clear_uploads();
        pending.publish_native();
        assert_eq!(g.ctx.upload_stats(), (0, 0), "healthy clear rejects old pending work");
        let mut fresh = Enc::with_owner(&g.ctx, cache.gpu_owner());
        fresh.keep_native(owner.clone(), 7, token.clone(), vec![1], image.clone());
        fresh.publish_native();
        assert_eq!(g.ctx.upload_stats(), (1, 1024), "healthy clear permits fresh retention");
        let gate = Arc::new(AtomicBool::new(false));
        cache.set_gate(Some(gate.clone()));
        assert!(!token.valid());
        let gated = cache.resident_token().unwrap();
        gate.store(true, Ordering::Relaxed);
        assert!(!gated.valid());
        assert!(cache.resident_token().is_none());
        gate.store(false, Ordering::Relaxed);
        let mut e = Enc::with_owner(&g.ctx, cache.gpu_owner());
        e.keep_native(owner.clone(), 7, gated.clone(), vec![1], image);
        e.publish_native();
        let old_owner = cache.gpu_owner();
        drop(cache);
        assert!(!gated.valid(), "cache lifetime does not survive in-flight tokens");
        assert!(g.ctx.native_get(&owner, 7, &gated, &[1], old_owner.as_ref()).is_none());
        g.ctx.set_cache_budget(1);
        assert_eq!(g.ctx.upload_stats(), (0, 0));
        g.ctx.set_cache_budget(1 << 30);
    }

    #[test]
    fn native_yuv_cache_reuses_processed_pixels_and_invalidates() {
        use crate::tests::{Scene, set, v3};
        use effectcraft_keyframe::Value;
        use effectcraft_project::BitDepth;
        use effectcraft_render::{Backend, RenderOpts, Renderer};
        use effectcraft_time::Tick;
        let Some(g) = crate::tests::gpu() else { return };
        g.ctx.clear_uploads();
        let source = StableNative(std::sync::Arc::new(NativeFixture::new()));
        let cache = effectcraft_render::LayerCache::new(1 << 20);
        g.ctx.set_cache_budget(1 << 20);
        let mut scene = Scene::new(BitDepth::Bpc8);
        let mut layer = scene.footage(97, 61);
        scene.effect(&mut layer, "ec.color.exposure", &[("master/exposure", Value::Scalar(0.2))]);
        let id = layer.id;
        scene.push(layer);
        let render = |scene: &Scene| {
            let mut r = Renderer::new(&scene.p, &source, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.cache = Some(&cache);
            r.accel = Some(g);
            let before = g.ctx.transfer_stats();
            let image = g.render(&r, scene.cid, Tick::ZERO).unwrap();
            let after = g.ctx.transfer_stats();
            let cpu = Renderer::new(&scene.p, &source, RenderOpts::default()).comp_frame_cpu(scene.cid, Tick::ZERO);
            assert_eq!(crate::tests::diff(&cpu, &image, crate::tests::tolerance(BitDepth::Bpc8)).over, 0);
            (image, after.yuv_conversions - before.yuv_conversions, after.upload_bytes - before.upload_bytes)
        };
        let (first, conversions, _) = render(&scene);
        assert_eq!(conversions, 1);
        // Kept once drawn again (playback never shows a video frame twice).
        let (again, conversions, _) = render(&scene);
        assert_eq!(first.data, again.data);
        assert_eq!(conversions, 1);
        let (second, conversions, bytes) = render(&scene);
        assert_eq!(first.data, second.data);
        assert_eq!((conversions, bytes), (0, 0));
        // Placement is excluded from the cached source but still changes output.
        let layer = scene.p.comp_mut(scene.cid).unwrap().layer_mut(id).unwrap();
        set(layer, "transform/position", v3(37.0, 26.0));
        let (moved, conversions, bytes) = render(&scene);
        assert_ne!(moved.data, first.data);
        assert_eq!((conversions, bytes), (0, 0));
        cache.clear();
        assert_eq!(render(&scene).1, 1, "clear invalidates processed GPU pixels");
        g.ctx.set_cache_budget(1);
        assert_eq!(g.ctx.upload_stats(), (0, 0));
        assert_eq!(render(&scene).1, 1);
        assert_eq!(render(&scene).1, 1);
        g.ctx.set_cache_budget(1 << 30);
        g.ctx.clear_uploads();
        let display = |scene: &Scene| {
            let mut r = Renderer::new(&scene.p, &source, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.cache = Some(&cache);
            r.accel = Some(g);
            let before = g.ctx.transfer_stats();
            let frame = g.render_display(&r, scene.cid, Tick::ZERO).unwrap();
            let bytes = g.read_display(&frame).unwrap();
            let after = g.ctx.transfer_stats();
            let cpu = Renderer::new(&scene.p, &source, RenderOpts::default()).comp_frame_cpu(scene.cid, Tick::ZERO);
            assert_eq!(bytes.len(), cpu.data.len() * 4);
            for (actual, reference) in bytes.iter().zip(cpu.data.iter().flatten()) {
                let expected = (reference.clamp(0.0, 1.0) * 255.0).round() as u8;
                assert!(actual.abs_diff(expected) <= 1);
            }
            (bytes, after.yuv_conversions - before.yuv_conversions, after.upload_bytes - before.upload_bytes)
        };
        let (first, conversions, _) = display(&scene);
        assert_eq!(conversions, 1);
        let (same, conversions, bytes) = display(&scene);
        assert_eq!(same, first);
        assert_eq!((conversions, bytes), (0, 0));
        set(scene.p.comp_mut(scene.cid).unwrap().layer_mut(id).unwrap(), "transform/position", v3(42.0, 31.0));
        let (moved, conversions, bytes) = display(&scene);
        assert_ne!(moved, first);
        assert_eq!((conversions, bytes), (0, 0));
        g.ctx.clear_uploads();
    }

    #[test]
    fn native_yuv_resident_full_half_mask_linear_and_effects() {
        use crate::tests::{Scene, set, v3};
        use effectcraft_keyframe::{ShapePath, Value};
        use effectcraft_project::{BitDepth, MaskMode};
        use effectcraft_render::{Backend, RenderOpts, Renderer};
        use effectcraft_time::Tick;
        let Some(g) = crate::tests::gpu() else { return };
        let source = NativeFixture::new();
        for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
            for scale in [1.0, 0.5, 0.25, 0.37] {
                // A half-sized provider frame also exercises proxy placement.
                for nominal in [1, 2] {
                    for linear in [false, true] {
                        let mut scene = Scene::new(depth);
                        if linear {
                            scene.p.settings.working_space = Some(effectcraft_color::ColorSpace::Srgb);
                            scene.p.settings.linearize = true;
                        }
                        let bg = scene.solid([0.13, 0.23, 0.32], 97, 61);
                        scene.push(bg);
                        let mut layer = scene.footage(97 * nominal, 61 * nominal);
                        set(&mut layer, "transform/position", v3(43.0, 27.0));
                        let mut next = scene.p.next_id;
                        let mask = effectcraft_project::build::mask(
                            &mut effectcraft_project::build::Ids(&mut next),
                            "mask",
                            ShapePath::ellipse([44.0, 28.0], 71.0, 43.0),
                            MaskMode::Add,
                            [255, 255, 0],
                        );
                        scene.p.next_id = next;
                        layer.props.sub_mut("masks").unwrap().children.push(mask.into());
                        scene.effect(&mut layer, "ec.color.exposure", &[("master/exposure", Value::Scalar(0.2))]);
                        layer.preserve_transparency = true;
                        scene.push(layer);
                        let opts = RenderOpts { scale, backend: Backend::Cpu, ..Default::default() };
                        let cpu = Renderer::new(&scene.p, &source, opts).comp_frame_cpu(scene.cid, Tick::ZERO);
                        let mut renderer = Renderer::new(&scene.p, &source, RenderOpts { backend: Backend::Gpu, ..opts });
                        renderer.accel = Some(g);
                        let before = g.ctx.transfer_stats();
                        let gpu = g.render(&renderer, scene.cid, Tick::ZERO).expect("resident frame");
                        let after = g.ctx.transfer_stats();
                        if (linear && depth == BitDepth::Bpc8) || scale == 0.37 {
                            assert_eq!(after.yuv_conversions - before.yuv_conversions, 0, "unvalidated source interpretation retains CPU conversion");
                        } else {
                            assert_eq!(after.readbacks - before.readbacks, 1, "only final frame readback");
                            assert_eq!(after.yuv_conversions - before.yuv_conversions, 1, "native conversion used");
                        }
                        let diff = crate::tests::diff(&cpu, &gpu, crate::tests::tolerance(depth));
                        if (linear && depth == BitDepth::Bpc8) || scale == 0.37 {
                            // This path deliberately declines native conversion. Compare the
                            // existing GPU path exactly, retaining its CPU-oracle diagnostic.
                            let mut fallback_source = source.clone();
                            fallback_source.native = false;
                            let mut fallback_renderer = Renderer::new(&scene.p, &fallback_source, RenderOpts { backend: Backend::Gpu, ..opts });
                            fallback_renderer.accel = Some(g);
                            let fallback = g.render(&fallback_renderer, scene.cid, Tick::ZERO).expect("fallback frame");
                            assert_eq!(gpu.data, fallback.data, "declined native path must be unchanged");
                            eprintln!("existing fallback CPU/GPU difference scale{scale}: {diff:?}");
                        } else {
                            assert_eq!(diff.over, 0, "{depth:?} scale{scale} linear{linear}: {diff:?}");
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn native_yuv_cpu_effect_boundary_and_unavailable_pipeline_fallback() {
        use crate::tests::Scene;
        use effectcraft_keyframe::Value;
        use effectcraft_project::BitDepth;
        use effectcraft_render::{Backend, RenderOpts, Renderer};
        use effectcraft_time::Tick;
        if crate::tests::gpu().is_none() {
            return;
        }
        let source = NativeFixture::new();
        for unavailable_yuv in [false, true] {
            let mut ctx = pollster::block_on(crate::context::GpuContext::request()).expect("native GPU");
            if unavailable_yuv {
                ctx.yuv = None;
            } else {
                ctx.drop_kernel("box_h");
                ctx.drop_kernel("box_delta");
            }
            let g = crate::Gpu::from_context(ctx);
            for depth in [BitDepth::Bpc8, BitDepth::Bpc32] {
                let mut scene = Scene::new(depth);
                let mut layer = scene.footage(97, 61);
                scene.effect(&mut layer, "ec.blur.gaussian", &[("blurriness", Value::Scalar(20.0))]);
                scene.effect(&mut layer, "ec.color.exposure", &[("master/exposure", Value::Scalar(0.1))]);
                scene.push(layer);
                let opts = RenderOpts { backend: Backend::Cpu, scale: 0.5, ..Default::default() };
                let cpu = Renderer::new(&scene.p, &source, opts).comp_frame_cpu(scene.cid, Tick::ZERO);
                let mut renderer = Renderer::new(&scene.p, &source, RenderOpts { backend: Backend::Gpu, ..opts });
                renderer.accel = Some(&g);
                let before = g.ctx.transfer_stats();
                let gpu = g.within_memory(|| g.render(&renderer, scene.cid, Tick::ZERO)).expect("guarded frame").expect("GPU frame");
                let after = g.ctx.transfer_stats();
                if !unavailable_yuv {
                    assert!(after.readbacks - before.readbacks >= 2, "CPU blur boundary and final frame readback");
                }
                let diff = crate::tests::diff(&cpu, &gpu, crate::tests::tolerance(depth));
                assert_eq!(diff.over, 0, "{depth:?} unavailableYuv{unavailable_yuv}: {diff:?}");
            }
        }
    }
}
