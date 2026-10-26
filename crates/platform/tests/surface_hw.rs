//! HWDEC2: the GPU reads a hardware-decoded picture straight from the decoder's surface. The
//! surface's samples, read on the GPU, are exactly the planar samples the decoder copied for the
//! CPU; surfaces are only found while their frame lives; at most `MAX_SURFACES` are kept (macOS
//! with Metal and a hardware decoder; elsewhere the test notes why and passes). Its own test
//! binary: the surface registry is process-wide.

#![cfg(target_os = "macos")]

mod nal;

use effectcraft_platform::h26x::VtNalDecoder;
use effectcraft_platform::nal::select;
use effectcraft_platform::surfaces::{self, MAX_SURFACES};
use filmcraft_codecs::{DecodedFrame, VideoDecoder};
use filmcraft_frame::PixelData;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
}

/// The samples of an integer texture (`R8Uint` / `R16Uint`: one channel; `Rg*Uint`: two),
/// `w`×`h`, read on the GPU (the imported textures can only be sampled, not copied).
fn read(device: &wgpu::Device, queue: &wgpu::Queue, t: &wgpu::Texture, channels: u32) -> Vec<u32> {
    let (w, h) = (t.width(), t.height());
    let n = (w * h * channels) as u64;
    let src = format!(
        "@group(0) @binding(0) var t: texture_2d<u32>;
         @group(0) @binding(1) var<storage, read_write> out: array<u32>;
         @compute @workgroup_size(8, 8) fn main(@builtin(global_invocation_id) id: vec3<u32>) {{
             if (id.x >= {w}u || id.y >= {h}u) {{ return; }}
             let v = textureLoad(t, vec2<i32>(id.xy), 0);
             let i = (id.y * {w}u + id.x) * {channels}u;
             out[i] = v.r;
             if ({channels}u > 1u) {{ out[i + 1u] = v.g; }}
         }}"
    );
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: None, source: wgpu::ShaderSource::Wgsl(src.into()) });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: None,
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let storage = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: n * 4,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: n * 4,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let view = t.create_view(&Default::default());
    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: storage.as_entire_binding() },
        ],
    });
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
    }
    enc.copy_buffer_to_buffer(&storage, 0, &staging, 0, n * 4);
    queue.submit([enc.finish()]);
    staging.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None }).expect("poll");
    let out = bytemuck_u32(&staging.slice(..).get_mapped_range().expect("mapped"));
    staging.unmap();
    out
}

fn bytemuck_u32(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// The frame's luma address (the registry key) and its planes as integers.
fn planes(f: &DecodedFrame) -> (usize, [Vec<u32>; 3], u32) {
    match &f.frame.data {
        PixelData::Yuv8 { planes, .. } => (planes[0].as_ptr() as usize, planes.each_ref().map(|p| p.iter().map(|&v| u32::from(v)).collect()), 8),
        PixelData::Yuv16 { planes, bits, .. } => (planes[0].as_ptr() as usize, planes.each_ref().map(|p| p.iter().map(|&v| u32::from(v)).collect()), *bits),
        _ => panic!("not planar YUV"),
    }
}

#[test]
fn the_gpu_reads_exactly_the_decoded_samples_from_the_decoder_surface() {
    let Some((device, queue)) = device() else {
        eprintln!("no Metal device: skipping");
        return;
    };
    // the first ask turns keeping surfaces on (nothing is kept before)
    assert!(surfaces::import(&device, 1, 2, 2, 8).is_none());
    for (name, s) in [("H.264", nal::h264(194, 110, 24, 2, 8)), ("HEVC Main 10", nal::hevc(192, 108, 6, true, 3))] {
        let stream = select(&s.entry, true, |_| true).expect("taken");
        let mut d = match VtNalDecoder::new(stream) {
            Ok(d) => d,
            Err(why) => {
                eprintln!("{name}: no hardware session ({why}): skipping");
                continue;
            }
        };
        let mut frames = Vec::new();
        for (data, pts, _) in &s.samples {
            frames.extend(d.decode(data, *pts).expect("decode"));
        }
        frames.extend(d.flush());
        assert!(surfaces::kept() <= MAX_SURFACES, "{name}: bounded");
        let recent = &frames[frames.len().saturating_sub(MAX_SURFACES / 2)..];
        let mut imported = 0;
        for f in recent {
            let (address, cpu, bits) = planes(f);
            let (w, h) = (f.frame.width, f.frame.height);
            let Some(p) = surfaces::import(&device, address, w, h, bits) else { continue };
            imported += 1;
            // the picture's window of the surface, row by row
            let window = |t: &wgpu::Texture, channels: u32, c: u32, (x0, y0): (u32, u32), (ww, hh): (u32, u32)| -> Vec<u32> {
                let all = read(&device, &queue, t, channels);
                let tw = t.width();
                (y0..y0 + hh).flat_map(|y| (x0..x0 + ww).map(move |x| ((y * tw + x) * channels + c) as usize)).map(|i| all[i] >> p.shift).collect()
            };
            let (ox, oy) = p.origin;
            assert_eq!(window(&p.luma, 1, 0, (ox, oy), (w, h)), cpu[0], "{name}: frame {}: luma read from the surface", f.pts);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            assert_eq!(window(&p.chroma, 2, 0, (ox / 2, oy / 2), (cw, ch)), cpu[1], "{name}: frame {}: Cb read from the surface", f.pts);
            assert_eq!(window(&p.chroma, 2, 1, (ox / 2, oy / 2), (cw, ch)), cpu[2], "{name}: frame {}: Cr read from the surface", f.pts);
            // a size or depth that doesn't match is never served
            assert!(surfaces::import(&device, address, w + 2, h, bits).is_none());
        }
        assert!(imported > 0, "{name}: the recent frames' surfaces are kept");
        // once the frames are gone their surfaces are never found again
        let keys: Vec<_> = frames.iter().map(|f| (planes(f).0, f.frame.width, f.frame.height, planes(f).2)).collect();
        drop(frames);
        for (address, w, h, bits) in keys {
            assert!(surfaces::import(&device, address, w, h, bits).is_none(), "{name}: a dropped frame's surface");
        }
    }
}
