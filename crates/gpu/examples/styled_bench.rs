//! `cargo run -p effectcraft-gpu --example styled_bench -- <project.ecproj> [--frames N]
//! [--comp NAME@SECONDS]...`
//!
//! Plays comps of a project on the GPU through one layer cache, as the viewer does, and prints
//! the median frame time and the slowest layers' median times (`Renderer::times`). Made for the
//! layer-styled card bodies (Noise + Bevel Alpha + Inner Shadow) of Elias's Tapa comps; run it
//! with and without `EFFECTCRAFT_GPU_STYLED_RESIDENT=0` in turn to compare. A build outside
//! the test harness: no NaN-filled scratch images. Footage is a flat grey still per item (the
//! timings measure the comps' own layers, not decoding). Defaults: S5 04 Four questions at 1 s
//! and HV1 01 Calendar at 6 s, 8 frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use effectcraft_gpu::Gpu;
use effectcraft_project::{Footage, ItemId, LayerId, Project};
use effectcraft_raster::Image;
use effectcraft_render::{Backend, FootageSource, LayerCache, LayerTimes, RenderOpts, Renderer};
use effectcraft_time::{FrameRate, Tick};

/// One flat still per item and size, made once.
struct Stills(Mutex<HashMap<(u64, u32, u32), Arc<Image>>>);

impl FootageSource for Stills {
    fn frame(&self, item: ItemId, f: &Footage, _t: Tick) -> Option<Arc<Image>> {
        let mut m = self.0.lock().ok()?;
        let img = m.entry((item.0, f.width, f.height)).or_insert_with(|| Arc::new(Image::filled(f.width.max(1), f.height.max(1), [0.4, 0.42, 0.45, 1.0])));
        Some(img.clone())
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().ok_or("usage: styled_bench <project.ecproj> [--frames N] [--comp NAME@SECONDS]")?;
    let frames: i64 = args.iter().position(|a| a == "--frames").and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(8);
    let mut comps: Vec<(String, f64)> =
        args.windows(2).filter(|w| w[0] == "--comp").filter_map(|w| w[1].rsplit_once('@').and_then(|(n, t)| Some((n.to_string(), t.parse().ok()?)))).collect();
    if comps.is_empty() {
        comps = vec![("S5 04 Four questions".into(), 1.0), ("HV1 01 Calendar".into(), 6.0)];
    }
    let json = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let p = Project::from_json(&json).map_err(|e| e.to_string())?;
    let g = Gpu::try_headless()?;
    let mode = if std::env::var("EFFECTCRAFT_GPU_STYLED_RESIDENT").is_ok_and(|v| v == "0") { "old way" } else { "kept on GPU" };
    for (name, at) in comps {
        let Some((&cid, _)) = p.items.iter().find(|(id, it)| it.name == name && p.comp(**id).is_some()) else {
            eprintln!("{name}: no such comp");
            continue;
        };
        let rate = p.comp(cid).map_or(FrameRate::FPS_30, |c| c.frame_rate);
        let from = rate.frame_at(Tick::from_seconds_f64(at));
        let stills = Stills(Mutex::new(HashMap::new()));
        let cache = LayerCache::default();
        // The frame before warms the cache (static layers, kernels).
        {
            let mut r = Renderer::new(&p, &stills, RenderOpts { backend: Backend::Gpu, ..Default::default() });
            r.accel = Some(&g);
            r.cache = Some(&cache);
            let _ = g.render(&r, cid, rate.tick_of(from - 1));
        }
        let mut ms = vec![];
        let mut layers: HashMap<(ItemId, LayerId), Vec<f64>> = HashMap::new();
        for f in from..from + frames {
            let times = LayerTimes::default();
            {
                let mut r = Renderer::new(&p, &stills, RenderOpts { backend: Backend::Gpu, ..Default::default() });
                r.accel = Some(&g);
                r.cache = Some(&cache);
                r.times = Some(&times);
                let t0 = Instant::now();
                let _ = g.render(&r, cid, rate.tick_of(f));
                ms.push(t0.elapsed().as_secs_f64() * 1e3);
            }
            for (k, v) in times.into_map() {
                layers.entry(k).or_default().push(v);
            }
        }
        let layer_name =
            |(c, l): (ItemId, LayerId)| p.comp(c).and_then(|c| c.layers.iter().find(|x| x.id == l)).map_or_else(|| format!("{l:?}"), |x| x.name.clone());
        let mut top: Vec<(String, f64)> = layers.into_iter().map(|(k, v)| (layer_name(k), median(v))).collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        top.truncate(5);
        let top: Vec<String> = top.iter().map(|(n, ms)| format!("{n} {ms:.1}")).collect();
        println!("{name} @{at}s {mode}: {:.1} ms/frame; layers: {}", median(ms), top.join(", "));
    }
    Ok(())
}
