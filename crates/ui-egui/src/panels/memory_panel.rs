//! Window ▸ Memory, and the RAM chip in the Timeline footer (next to Frame Render Time).
//! See [`effectcraft_engine::memory_map`] and plan/memory/memory-map.md.
//!
//! Cost: the chip reads three atomics at most twice a second (whenever the UI draws anyway; it
//! never asks for a repaint), starts a background reading of the process size at most every 2 s
//! and reads the GPU compositor's totals every 10 s. The panel samples the full map twice a
//! second only while it is shown (a hidden tab isn't drawn, so nothing runs).

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::{Align2, Color32, Rect, Sense, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::json;

use effectcraft_engine::memory_map::{self, Chip, Level, MemoryStats, Row};

use super::panel_kit as kit;
use crate::EffectcraftApp;
use crate::theme::Tokens;

/// The panel's own state (in [`crate::state::UiState`], so `ui.set {memory: …}` drives it).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemoryView {
    pub sort: MemorySort,
    /// Rows twirled open: `c<comp>` and `l<comp>.<layer>`.
    pub open: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MemorySort {
    #[default]
    Bytes,
    Name,
    Frames,
}

impl MemorySort {
    const ALL: [MemorySort; 3] = [MemorySort::Bytes, MemorySort::Name, MemorySort::Frames];
    fn label(self) -> &'static str {
        match self {
            MemorySort::Bytes => "Sort by Size",
            MemorySort::Name => "Sort by Name",
            MemorySort::Frames => "Sort by Frames",
        }
    }
}

/// Chip numbers are sampled at most this often (s).
const CHIP_EVERY: f64 = 0.5;
/// The process's size is read at most this often (s; each reading runs `ps` on macOS).
const PROCESS_EVERY: f64 = 2.0;
/// The GPU totals are read this often while the panel is closed (s).
const GPU_EVERY_CLOSED: f64 = 10.0;
/// The panel refreshes this often while shown (s).
const PANEL_EVERY: f64 = 0.5;

const GIB: f64 = (1u64 << 30) as f64;

/// `12.3 GB`, `512 MB`, `64 KB`.
pub fn human(b: u64) -> String {
    let f = b as f64;
    if f >= GIB {
        format!("{:.2} GB", f / GIB)
    } else if b >= 1 << 20 {
        format!("{:.0} MB", f / f64::from(1u32 << 20))
    } else {
        format!("{:.0} KB", f / 1024.0)
    }
}

/// The chip's text: `RAM 10.2 / 54 GB` (`(tracked)` when the process size is unknown).
pub fn chip_text(c: &Chip) -> String {
    let used = c.used as f64 / GIB;
    let mut s = match c.cap {
        Some(cap) => format!("RAM {used:.1} / {:.0} GB", cap as f64 / GIB),
        None => format!("RAM {used:.1} GB"),
    };
    if c.tracked {
        s.push_str(" (tracked)");
    }
    s
}

fn level_color(level: Level, t: &Tokens) -> Color32 {
    match level {
        Level::Ok => t.cache_green,
        Level::Warn => t.warning,
        Level::High => t.danger,
    }
}

/// Lend the RAM preview to the memory map (app construction): `memory.stats` and
/// `memory.purge` then see it even while no panel is open.
pub fn install(app: &mut EffectcraftApp) {
    let (probe, purge) = app.frames.memory_hooks();
    let meter = app.frames.memory_meter();
    let h = &mut app.session.memory_map;
    h.preview = Some(probe);
    h.preview_purge = Some(purge);
    h.preview_meter = Some(meter);
    // Effect scratch: the GPU compositor's working images, counted process-wide by the gpu
    // crate (two atomics; the engine only reads them).
    let scratch = effectcraft_gpu::effect_scratch();
    h.effect_scratch = Some(scratch.live_counter());
    h.effect_scratch_peak = Some(scratch.peak_counter());
}

/// The GPU compositor's memory now (pooled and leased textures, uploads, staging), less the
/// working images the "Effect scratch" category counts live (a subset of `leased`).
fn gpu_bytes(app: &EffectcraftApp) -> u64 {
    let Some(g) = &app.gpu else { return 0 };
    let r = g.context().vram_report(false);
    let leased = (r.leased as u64).saturating_sub(effectcraft_gpu::effect_scratch().bytes().0);
    [leased, r.pool_free as u64, r.pool_released as u64, r.zeros, r.planes, r.staging].into_iter().fold(0, u64::saturating_add)
}

/// Keep the process reading and the GPU meter current (throttled by egui time). Only desktop
/// sessions that read the system's memory poll the process (tests and headless runs stay quiet).
fn tick(app: &mut EffectcraftApp, ctx: &egui::Context, panel_open: bool) {
    let now = ctx.input(|i| i.time);
    let id = egui::Id::new("memory-map-tick");
    let (last_process, last_gpu): (f64, f64) = ctx.data(|d| d.get_temp(id)).unwrap_or((f64::NEG_INFINITY, f64::NEG_INFINITY));
    let process = app.watch_system && app.session.sys_memory.is_some() && now - last_process >= PROCESS_EVERY;
    if process {
        app.session.memory_map.process.poll();
    }
    let gpu_every = if panel_open { PANEL_EVERY } else { GPU_EVERY_CLOSED };
    let gpu = now - last_gpu >= gpu_every;
    if gpu {
        let b = gpu_bytes(app);
        app.session.memory_map.gpu_meter.store(b, std::sync::atomic::Ordering::Relaxed);
    }
    if process || gpu {
        ctx.data_mut(|d| d.insert_temp(id, (if process { now } else { last_process }, if gpu { now } else { last_gpu })));
    }
}

/// The RAM chip, right-aligned at `right` (vertical centre), not left of `min_x`; clicking it
/// opens Window ▸ Memory. Hidden while the machine's memory is unknown (headless runs, tests).
pub fn chip(app: &mut EffectcraftApp, ui: &mut egui::Ui, p: &egui::Painter, right: egui::Pos2, min_x: f32) {
    if app.session.sys_memory.is_none() {
        return;
    }
    let ctx = ui.ctx().clone();
    tick(app, &ctx, false);
    let now = ctx.input(|i| i.time);
    let id = egui::Id::new("memory-chip");
    let cached: Option<(f64, Chip)> = ctx.data(|d| d.get_temp(id));
    let c = match cached {
        Some((at, c)) if now - at < CHIP_EVERY => c,
        _ => {
            let c = memory_map::chip(&app.session);
            ctx.data_mut(|d| d.insert_temp(id, (now, c)));
            c
        }
    };
    let t = app.tokens;
    let text = chip_text(&c);
    let galley = p.layout_no_wrap(text.clone(), Tokens::ui(11.0), t.text_dim);
    let w = galley.size().x + 18.0;
    let r = Rect::from_min_size(pos2(right.x - w, right.y - 9.0), vec2(w, 18.0));
    if r.min.x < min_x {
        return;
    }
    let resp = ui.interact(r, egui::Id::new("timeline-memory-chip"), Sense::click()).on_hover_text(crate::i18n::tr(
        "This app's memory against what it may use (installed memory minus RAM Reserved for Other Applications). Click for Window ▸ Memory.",
    ));
    let color = level_color(c.level, &t);
    p.rect_filled(r, 9.0, if resp.hovered() { t.hover } else { t.field_bg });
    p.circle_filled(pos2(r.min.x + 8.0, r.center().y), 3.5, color);
    p.galley(pos2(r.min.x + 14.0, r.center().y - galley.size().y / 2.0), galley, t.text_dim);
    app.auto.add("timeline.memoryChip", r, &text);
    if resp.clicked() {
        kit::exec(app, "memory.show", json!({}));
    }
}

/// The map, refreshed at most every [`PANEL_EVERY`] while the panel is shown.
fn sampled(app: &mut EffectcraftApp, ctx: &egui::Context) -> Arc<MemoryStats> {
    tick(app, ctx, true);
    let now = ctx.input(|i| i.time);
    let id = egui::Id::new("memory-panel-stats");
    let cached: Option<(f64, Arc<MemoryStats>)> = ctx.data(|d| d.get_temp(id));
    if let Some((at, s)) = cached
        && now - at < PANEL_EVERY
    {
        return s;
    }
    let s = Arc::new(memory_map::stats(&app.session, true));
    ctx.data_mut(|d| d.insert_temp(id, (now, s.clone())));
    s
}

fn sort_rows(rows: &mut [Row], sort: MemorySort) {
    match sort {
        MemorySort::Bytes => rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id))),
        MemorySort::Name => rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id))),
        MemorySort::Frames => rows.sort_by(|a, b| b.frames.cmp(&a.frames).then(b.entries.cmp(&a.entries)).then(b.bytes.cmp(&a.bytes))),
    }
}

/// Window ▸ Memory.
pub fn show(app: &mut EffectcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let ctx = ui.ctx().clone();
    let stats = sampled(app, &ctx);
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(PANEL_EVERY));
    let scroll = crate::widgets::PanelScroll::begin(ui, egui::Id::new("memory-panel-scroll"), rect);
    let old_clip = ui.clip_rect();
    ui.set_clip_rect(old_clip.intersect(rect));
    let t = app.tokens;
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, t.panel_bg);
    let x = rect.min.x + 10.0;
    let width = (rect.width() - 20.0).max(1.0);
    let start = rect.min.y + 10.0 - scroll.offset;
    let mut y = start;

    // Headline: the chip's numbers, in words.
    let chip = stats.chip;
    painter.circle_filled(pos2(x + 5.0, y + 8.0), 5.0, level_color(chip.level, &t));
    painter.text(pos2(x + 16.0, y), Align2::LEFT_TOP, chip_text(&chip), Tokens::semibold(13.0), t.text);
    let pressure = match chip.pressure {
        Some(effectcraft_engine::sysinfo::MemoryPressure::Critical) => "memory pressure: critical",
        Some(effectcraft_engine::sysinfo::MemoryPressure::Warning) => "memory pressure: warning",
        Some(effectcraft_engine::sysinfo::MemoryPressure::Normal) => "memory pressure: normal",
        None => "memory pressure: unknown",
    };
    let what = if chip.tracked { "the caches' tracked bytes" } else { "resident size of this app" };
    painter.text(pos2(x, y + 20.0), Align2::LEFT_TOP, format!("{what} · {pressure}"), Tokens::ui(10.5), t.text_dim);
    y += 40.0;

    // Stacked bar per category, against the cap (or the total when the cap is unknown).
    let c = stats.categories;
    let parts: [(&str, u64, Color32); 6] = [
        ("RAM preview", c.preview, t.cache_green),
        ("Layer cache", c.layer, t.accent),
        ("Media / decoders", c.media, t.cache_blue),
        ("GPU textures", c.gpu, t.keyframe),
        ("Effect scratch", c.effect_scratch.unwrap_or(0), t.warning),
        ("Untracked", c.untracked.unwrap_or(0), t.text_faint),
    ];
    let sum = parts.iter().map(|p| p.1).fold(0u64, u64::saturating_add);
    let scale = chip.cap.unwrap_or(sum).max(sum).max(1) as f32;
    let bar = Rect::from_min_size(pos2(x, y), vec2(width, 16.0));
    painter.rect_filled(bar, 3.0, t.field_bg);
    let mut bx = bar.min.x;
    for (_, b, col) in parts {
        let w = bar.width() * (b as f32 / scale);
        if w > 0.5 {
            painter.rect_filled(Rect::from_min_size(pos2(bx, bar.min.y), vec2(w, bar.height())), 0.0, col);
        }
        bx += w;
    }
    app.auto.add("memory.bar", bar, "Memory by cache");
    y += 24.0;
    for (name, b, col) in parts {
        if name == "Effect scratch" && c.effect_scratch.is_none() {
            continue;
        }
        if name == "Untracked" && c.untracked.is_none() {
            continue;
        }
        painter.rect_filled(Rect::from_min_size(pos2(x, y + 3.0), vec2(9.0, 9.0)), 2.0, col);
        painter.text(pos2(x + 15.0, y), Align2::LEFT_TOP, name, Tokens::ui(11.0), t.text);
        let value = match stats.effect_scratch_peak {
            Some(peak) if name == "Effect scratch" => format!("{} (peak {})", human(b), human(peak)),
            _ => human(b),
        };
        painter.text(pos2(x + width, y), Align2::RIGHT_TOP, value, Tokens::mono(11.0), t.text_dim);
        y += 17.0;
    }
    let detail = format!(
        "Preview {} frames of {} · Layer cache {} buffers of {} · Media {} frames",
        stats.preview.frames,
        human(stats.preview.budget),
        stats.layer_cache_entries,
        human(stats.layer_cache_budget),
        stats.media_frames
    );
    painter.text(pos2(x, y + 2.0), Align2::LEFT_TOP, detail, Tokens::ui(10.5), t.text_faint);
    y += 24.0;
    // Share One Budget Between the Caches: who holds what of the shared total, and each floor.
    if let Some(sh) = &stats.shared_budget {
        let part = |name: &str, u: &effectcraft_render::cache::shared::ShareUsage| format!("{name} {} (floor {})", human(u.bytes), human(u.floor));
        let line = format!(
            "Shared budget {} of {}: {} · {} · {}",
            human(sh.used),
            human(sh.total),
            part("Preview", &sh.preview),
            part("Layers", &sh.layer),
            part("Media", &sh.media)
        );
        let r = painter.text(pos2(x, y - 4.0), Align2::LEFT_TOP, &line, Tokens::ui(10.5), t.text_faint);
        app.auto.add("memory.sharedBudget", r, &line);
        y += 18.0;
    }

    // Purge All and the sort order.
    let half = ((width - 6.0) / 2.0).max(1.0);
    if kit::button(app, ui, Rect::from_min_size(pos2(x, y), vec2(half, 22.0)), "memory.purgeAll", "Purge All", false) {
        kit::exec(app, "memory.purge", json!({}));
        ctx.data_mut(|d| d.remove_temp::<(f64, Arc<MemoryStats>)>(egui::Id::new("memory-panel-stats")));
    }
    let sort = app.ui.memory.sort;
    let labels: Vec<&str> = MemorySort::ALL.iter().map(|s| s.label()).collect();
    let cur = MemorySort::ALL.iter().position(|s| *s == sort).unwrap_or(0);
    if let Some(i) = kit::dropdown(app, ui, Rect::from_min_size(pos2(x + half + 6.0, y), vec2(half, 22.0)), "memory.sort", &labels, cur)
        && let Some(s) = MemorySort::ALL.get(i)
    {
        app.ui.memory.sort = *s;
    }
    y += 32.0;

    // comp → layer → effect.
    painter.text(pos2(x, y), Align2::LEFT_TOP, "By composition", Tokens::semibold(12.0), t.text);
    y += 20.0;
    let mut comps = stats.comps.clone();
    sort_rows(&mut comps, app.ui.memory.sort);
    if comps.is_empty() {
        painter.text(pos2(x, y), Align2::LEFT_TOP, "Nothing cached yet.", Tokens::ui(11.0), t.text_faint);
        y += 20.0;
    }
    for comp in &comps {
        let key = format!("c{}", comp.id);
        let open = app.ui.memory.open.contains(&key);
        y = row(app, ui, &painter, x, y, width, 0, comp, Some(open), &key, json!({"comp": comp.id}));
        if !open {
            continue;
        }
        let mut layers = comp.children.clone();
        sort_rows(&mut layers, app.ui.memory.sort);
        for layer in &layers {
            let lkey = format!("l{}.{}", comp.id, layer.id);
            let lopen = app.ui.memory.open.contains(&lkey);
            let twirl = (!layer.children.is_empty()).then_some(lopen);
            y = row(app, ui, &painter, x, y, width, 1, layer, twirl, &lkey, json!({"comp": comp.id, "layer": layer.id}));
            if !lopen {
                continue;
            }
            for fx in &layer.children {
                // An effect's input is purged with its layer (the cache doesn't key inputs apart).
                y = row(app, ui, &painter, x, y, width, 2, fx, None, &format!("e{}.{}.{}", comp.id, layer.id, fx.id), serde_json::Value::Null);
            }
        }
    }
    if let Some(u) = stats.unattributed.as_ref().filter(|u| u.bytes > 0) {
        y = row(app, ui, &painter, x, y, width, 0, u, None, "unattributed", serde_json::Value::Null);
    }

    // Decoded footage by file.
    if !stats.media_files.is_empty() {
        y += 10.0;
        painter.text(pos2(x, y), Align2::LEFT_TOP, "Decoded footage", Tokens::semibold(12.0), t.text);
        y += 20.0;
        for (path, bytes, frames) in stats.media_files.iter().take(12) {
            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
            painter.text(pos2(x + 4.0, y), Align2::LEFT_TOP, name, Tokens::ui(11.0), t.text);
            painter.text(pos2(x + width, y), Align2::RIGHT_TOP, format!("{} · {frames} fr", human(*bytes as u64)), Tokens::mono(11.0), t.text_dim);
            y += 18.0;
        }
    }

    ui.set_clip_rect(old_clip);
    scroll.end(ui, &mut app.auto, "memory.scroll", y - start + 20.0, &t);
}

/// One tree row: twirl (when `open` is `Some`), name, size, frames / buffers, and a Purge
/// button when `purge` has params. Returns the next row's y.
#[allow(clippy::too_many_arguments)]
fn row(
    app: &mut EffectcraftApp,
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    x: f32,
    y: f32,
    width: f32,
    depth: u8,
    r: &Row,
    open: Option<bool>,
    key: &str,
    purge: serde_json::Value,
) -> f32 {
    let t = app.tokens;
    let h = 20.0;
    let indent = 14.0 * f32::from(depth);
    let purge_w = if purge.is_null() { 0.0 } else { 52.0 };
    let line = Rect::from_min_size(pos2(x, y), vec2(width, h));
    if depth == 0 {
        painter.rect_filled(line, 2.0, t.row_alt);
    }
    // Twirl + name: one click target.
    let name_r = Rect::from_min_max(pos2(x + indent, y), pos2((x + width - purge_w - 150.0).max(x + indent + 20.0), y + h));
    if let Some(is_open) = open {
        let resp = ui.interact(name_r, egui::Id::new(("memory-row", key)), Sense::click());
        painter.text(pos2(name_r.min.x + 2.0, y + h / 2.0), Align2::LEFT_CENTER, if is_open { "▾" } else { "▸" }, Tokens::ui(11.0), t.text_dim);
        app.auto.add(&format!("memory.row.{key}"), name_r, &r.name);
        if resp.clicked() {
            if is_open {
                app.ui.memory.open.remove(key);
            } else {
                app.ui.memory.open.insert(key.to_string());
            }
        }
    }
    let name_x = name_r.min.x + if open.is_some() { 14.0 } else { 2.0 };
    let name_clip = painter.with_clip_rect(painter.clip_rect().intersect(Rect::from_min_max(pos2(name_x, y), pos2(name_r.max.x, y + h))));
    name_clip.text(pos2(name_x, y + h / 2.0), Align2::LEFT_CENTER, &r.name, Tokens::ui(11.5), t.text);
    // Size, then frames (comps) or buffers.
    let size_right = x + width - purge_w - 6.0;
    painter.text(pos2(size_right, y + h / 2.0), Align2::RIGHT_CENTER, human(r.bytes), Tokens::mono(11.0), t.text);
    let count = if r.frames > 0 { format!("{} fr", r.frames) } else { format!("{} buf", r.entries) };
    painter.text(pos2(size_right - 70.0, y + h / 2.0), Align2::RIGHT_CENTER, count, Tokens::mono(10.5), t.text_dim);
    if !purge.is_null() {
        let br = Rect::from_min_size(pos2(x + width - purge_w + 2.0, y + 1.0), vec2(purge_w - 2.0, h - 2.0));
        if kit::button(app, ui, br, &format!("memory.purge.{key}"), "Purge", false) {
            kit::exec(app, "memory.purge", purge);
            ui.ctx().data_mut(|d| d.remove_temp::<(f64, Arc<MemoryStats>)>(egui::Id::new("memory-panel-stats")));
        }
    }
    y + h + 2.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_text_reads_like_the_design() {
        let gb = 1u64 << 30;
        let c = Chip { used: gb * 102 / 10, tracked: false, cap: Some(54 * gb), pressure: None, level: Level::Ok };
        assert_eq!(chip_text(&c), "RAM 10.2 / 54 GB");
        assert_eq!(chip_text(&Chip { cap: None, tracked: true, ..c }), "RAM 10.2 GB (tracked)");
        assert_eq!(human(3 << 20), "3 MB");
        assert_eq!(human(2048), "2 KB");
    }

    #[test]
    fn sorts_rows() {
        let r = |id: u64, name: &str, bytes: u64, frames: u64| Row { id, name: name.into(), bytes, frames, ..Row::default() };
        let mut rows = vec![r(1, "b", 10, 5), r(2, "A", 30, 1), r(3, "c", 20, 9)];
        sort_rows(&mut rows, MemorySort::Bytes);
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 3, 1]);
        sort_rows(&mut rows, MemorySort::Name);
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 1, 3]);
        sort_rows(&mut rows, MemorySort::Frames);
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![3, 1, 2]);
    }
}
