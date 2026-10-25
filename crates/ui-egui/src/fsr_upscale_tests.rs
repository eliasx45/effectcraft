#![cfg(all(test, not(target_arch = "wasm32")))]
//! UP1: when the viewer's FSR1 upscale runs, at what size, and what it writes.

use crate::fsr_upscale::{MAX_PIXELS, Upscaler, target_size};
use eframe::wgpu;

const LIMIT: u32 = 16384;

/// Half, Third and Quarter frames shown magnified upscale to their size on screen; Full frames,
/// frames at or under their own pixel size, and the pref off are left alone.
#[test]
fn upscales_only_low_resolution_frames_shown_magnified() {
    // Half of a 1920×1080 comp at 100 % on a 1× display: 2× to the comp's Full size.
    assert_eq!(target_size(true, [960, 540], 0.5, 1.0, LIMIT), Some([1920, 1080]));
    // Third and Quarter at 100 %.
    assert_eq!(target_size(true, [640, 360], 1.0 / 3.0, 1.0, LIMIT), Some([1920, 1080]));
    assert_eq!(target_size(true, [480, 270], 0.25, 1.0, LIMIT), Some([1920, 1080]));
    // Half at 50 % on a Retina display (2 pixels per point): 1 screen pixel per comp pixel.
    assert_eq!(target_size(true, [960, 540], 0.5, 1.0, LIMIT), target_size(true, [960, 540], 0.5, 0.5 * 2.0, LIMIT));
    // Quarter at 50 % on a 1× display: the size on screen, below Full.
    assert_eq!(target_size(true, [480, 270], 0.25, 0.5, LIMIT), Some([960, 540]));
    // Above 100 %: the comp's Full size × the zoom.
    assert_eq!(target_size(true, [960, 540], 0.5, 1.5, LIMIT), Some([2880, 1620]));
    // The pref off: blocks, as in After Effects (#882).
    assert_eq!(target_size(false, [960, 540], 0.5, 1.0, LIMIT), None);
    // Full resolution, however far it is magnified.
    assert_eq!(target_size(true, [1920, 1080], 1.0, 4.0, LIMIT), None);
    // Half shown at its own size (50 % on a 1× display) or smaller.
    assert_eq!(target_size(true, [960, 540], 0.5, 0.5, LIMIT), None);
    assert_eq!(target_size(true, [960, 540], 0.5, 0.25, LIMIT), None);
    // Barely magnified (Auto resolution rounding): not worth a pass.
    assert_eq!(target_size(true, [960, 540], 0.5, 0.504, LIMIT), None);
}

/// The output stays within the pixel budget and the device's texture limit, and nonsense never
/// asks for a pass.
#[test]
fn the_upscale_size_is_capped() {
    // A 4K comp at Half, 400 %: 8× would be 15360×8640 (132 MP); capped at MAX_PIXELS.
    let out = target_size(true, [1920, 1080], 0.5, 4.0, LIMIT).unwrap();
    assert!(f64::from(out[0]) * f64::from(out[1]) <= MAX_PIXELS * 1.001, "{out:?}");
    assert!(out[0] > 1920 && (f64::from(out[0]) / f64::from(out[1]) - 16.0 / 9.0).abs() < 0.01, "{out:?}");
    // A small texture limit.
    let out = target_size(true, [960, 540], 0.25, 1.0, 2048).unwrap();
    assert!(out[0] <= 2048 && out[1] <= 2048, "{out:?}");
    // Degenerate input.
    for (frame, scale, screen) in
        [([0, 540], 0.5, 1.0), ([960, 540], 0.0, 1.0), ([960, 540], f64::NAN, 1.0), ([960, 540], 0.5, f64::INFINITY), ([960, 540], 0.5, -1.0)]
    {
        assert_eq!(target_size(true, frame, scale, screen, LIMIT), None, "{frame:?} {scale} {screen}");
    }
    assert_eq!(target_size(true, [960, 540], 0.5, 1.0, 0), None);
}

fn device() -> Option<(wgpu::Adapter, wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).ok()?;
    Some((adapter, device, queue))
}

/// A viewer frame texture (as `effectcraft_gpu`'s display textures: premultiplied RGBA8, with
/// mips) holding `px` at level 0.
fn frame(device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, px: &[u8]) -> wgpu::Texture {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test frame"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 2,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        px,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    tex
}

/// Level 0 of `tex` as RGBA8 rows.
fn read(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (tex.width(), tex.height());
    let padded = (w * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: u64::from(padded * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(h) } },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = buf.slice(..).get_mapped_range().unwrap();
    data.chunks(padded as usize).flat_map(|row| row[..(w * 4) as usize].to_vec()).collect()
}

/// The pass upscales on the GPU: flat areas keep their colour, an edge stays within its two
/// sides (no ringing past them), colour never exceeds alpha, and the output texture is reused
/// while the size stays the same.
#[test]
fn easu_rcas_upscale_keeps_flat_areas_and_edges() {
    let Some((adapter, device, queue)) = device() else {
        eprintln!("fsr_upscale: no GPU device, skipping");
        return;
    };
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    // 16×8: the left half opaque dark blue, the right half half-transparent white (premultiplied).
    let (w, h) = (16u32, 8u32);
    let left = [10u8, 20, 90, 255];
    let right = [128u8, 128, 128, 128];
    let px: Vec<u8> = (0..h).flat_map(|_| (0..w).flat_map(|x| if x < w / 2 { left } else { right })).collect();
    let src = frame(&device, &queue, w, h, &px);
    let mut up = Upscaler::default();
    let view = up.run(&adapter, &device, &queue, &src, [48, 24]);
    assert!(view.is_some(), "the pass ran");
    assert_eq!(up.target_size(), Some([48, 24]));
    let out_tex = view.as_ref().map(wgpu::TextureView::texture).unwrap().clone();
    let out = read(&device, &queue, &out_tex);
    assert_eq!(out.len(), 48 * 24 * 4);
    for y in 0..24usize {
        for x in 0..48usize {
            let p = &out[(y * 48 + x) * 4..][..4];
            let a = p[3];
            assert!(p[0] <= a && p[1] <= a && p[2] <= a, "premultiplied at {x},{y}: {p:?}");
            for c in 0..4 {
                let (lo, hi) = (left[c].min(right[c]), left[c].max(right[c]));
                assert!(p[c] >= lo.saturating_sub(1) && p[c] <= hi.saturating_add(1), "within the edge's sides at {x},{y}: {p:?}");
            }
        }
        // Far from the edge, the flat colours come through.
        let at = |x: usize| out[(y * 48 + x) * 4..][..4].to_vec();
        for x in [0, 1, 2, 8, 15] {
            assert!(at(x).iter().zip(left).all(|(a, b)| a.abs_diff(b) <= 1), "left at {x},{y}: {:?}", at(x));
        }
        for x in [32, 40, 46, 47] {
            assert!(at(x).iter().zip(right).all(|(a, b)| a.abs_diff(b) <= 1), "right at {x},{y}: {:?}", at(x));
        }
    }
    // The same size again reuses the textures; another size makes new ones.
    let again = up.run(&adapter, &device, &queue, &src, [48, 24]).unwrap();
    assert_eq!(again.texture(), &out_tex, "reused");
    let bigger = up.run(&adapter, &device, &queue, &src, [64, 32]).unwrap();
    assert_ne!(bigger.texture(), &out_tex);
    assert_eq!(up.target_size(), Some([64, 32]));
    assert!(pollster::block_on(scope.pop()).is_none(), "no validation error");
}

/// A frame the pass can't read (another format) is drawn as it is, without a GPU error.
#[test]
fn an_unsupported_frame_is_left_alone() {
    let Some((adapter, device, queue)) = device() else { return };
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test frame (bgra)"),
        size: wgpu::Extent3d { width: 4, height: 4, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let mut up = Upscaler::default();
    assert!(up.run(&adapter, &device, &queue, &tex, [8, 8]).is_none());
    assert_eq!(up.target_size(), None);
}
