//! Sharp zoom (beyond After Effects, View ▸ Sharp Zoom, on by default): above 100 % the
//! Composition viewer also renders just the visible part of the comp at the magnification and
//! draws it over the magnified comp-resolution frame. Text, shapes and solids are drawn at their
//! size on screen, footage and nested comps are resampled from their own pixels, and effect sizes
//! in pixels scale with it (the renderer's buffer scale, as at Half or Quarter resolution).
//!
//! The comp-resolution frame is untouched (exports, RAM preview and every zoom up to 100 % use
//! only it): while a sharp tile renders, after an edit or a pan to where none was rendered, the
//! magnified frame shows, so panning stays smooth. One tile renders at a time on its own thread
//! (on the CPU, so it never competes with the GPU frames), the newest request next. Each layer
//! buffer stays within [`effectcraft_engine::render::SHARP_MAX_PIXELS`] and the tile within the
//! viewer (snapped to a coarse grid so small pans reuse it).

use std::sync::{Arc, Mutex};

use effectcraft_engine::project::{Comp, ItemId};
use effectcraft_engine::render::{Backend, ExprHost, FootageSource, RenderOpts, Renderer};
use effectcraft_engine::time::Tick;
use egui::{Color32, Rect, pos2, vec2};

use crate::EffectcraftApp;
use crate::frames::FrameKey;
use crate::panels::viewer::ViewerMap;

/// The most output pixels per comp pixel a tile is rendered at (past it the tile is magnified).
pub const MAX_SCALE: f64 = 16.0;
/// Tile edges snap to multiples of this many output pixels (small pans keep the tile).
const GRID: f64 = 256.0;
/// The most pixels a tile may have (a viewer on a very large display renders coarser).
const MAX_TILE_PIXELS: f64 = 48.0e6;

/// What a tile shows: the comp-resolution frame it sharpens (its content, frame, view and render
/// options), the output scale and the region (comp pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileKey {
    pub comp: u64,
    pub content: u64,
    pub frame: i64,
    pub view: u64,
    pub opts: u64,
    /// Output pixels per comp pixel × 1000.
    pub scale: u32,
    /// `[x, y, w, h]`, comp pixels.
    pub region: [f64; 4],
}

impl TileKey {
    /// The same pixels as `other` where both have them (only the region differs).
    fn same_series(&self, other: &TileKey) -> bool {
        TileKey { region: other.region, ..*self } == *other
    }

    pub fn scale(&self) -> f64 {
        f64::from(self.scale) / 1000.0
    }
}

/// A rendered tile on screen.
pub struct Tile {
    pub key: TileKey,
    pub image: Arc<egui::ColorImage>,
    tex: egui::TextureHandle,
}

/// A tile render's result, handed from its thread: the key and the pixels (`None`: it failed).
type Landed = Option<(TileKey, Option<Arc<egui::ColorImage>>)>;

/// The viewer's sharp-zoom state (on [`EffectcraftApp`]).
#[derive(Default)]
pub struct SharpZoom {
    /// The last finished tile.
    pub(crate) shown: Option<Tile>,
    /// The tile rendering now.
    running: Option<TileKey>,
    landed: Arc<Mutex<Landed>>,
}

/// Render the region `opts.roi` of `comp` at `t` at `opts.scale`, sharp (see [`sharp_opts`]): the
/// tile's pixels.
pub fn render_tile(project: &effectcraft_engine::project::Project, footage: &dyn FootageSource, expr: Option<&dyn ExprHost>, comp: ItemId, t: Tick, opts: RenderOpts) -> egui::ColorImage {
    let mut r = Renderer::new(project, footage, opts);
    r.expr = expr;
    crate::frames::to_color_image(&r.comp_frame(comp, t))
}

/// The render options of a sharp tile of `region` at `scale`, from the viewer frame's `base`
/// options: sharp zoom on, on the CPU, no layer cache (its buffers would crowd out the
/// comp-resolution frame's).
pub fn sharp_opts(base: RenderOpts, scale: f64, region: [f64; 4]) -> RenderOpts {
    RenderOpts { scale, roi: Some(region), sharp: true, backend: Backend::Cpu, ..base }
}

/// The scale and region of the tile the viewer wants now, or `None` (no sharp tile: Sharp Zoom
/// off, at or below 100 %, a lower Resolution, playing or scrubbing, a 3D comp, Show Channel /
/// exposure / display colour management, snapshots, wireframes, the cloud preview).
pub(crate) fn wanted(app: &EffectcraftApp, ctx: &egui::Context, map: &ViewerMap, comp: &Comp) -> Option<(f64, [f64; 4])> {
    if cfg!(target_arch = "wasm32") || !app.ui.viewer.sharp_zoom {
        return None;
    }
    let ppp = ctx.pixels_per_point();
    let m = f64::from(map.zoom * ppp);
    let v = &app.session.state.viewer;
    let plain = v.channel == effectcraft_engine::viewer::Channel::Rgb && v.exposure == 0.0 && effectcraft_engine::viewer::DisplayColor::of(&app.session).is_none();
    if !m.is_finite() || m <= 1.0 + 1e-3
        || app.viewer_res().factor().is_some_and(|n| n != 1)
        || app.playback.playing
        || app.scrubbing()
        || app.ui.viewer.interacting
        || app.ui.viewer.property_interacting
        || !plain
        || v.fast_previews == effectcraft_engine::commands::viewer_cmds::FastPreviews::Wireframe
        || crate::panels::viewer_tools::showing_snapshot(app, ctx)
        || crate::prefs_live::main_viewer_hidden(app)
        || app.frames.remote_active()
        || comp.has_3d()
    {
        return None;
    }
    let s = (m.min(MAX_SCALE) * 1000.0).round() / 1000.0;
    // What the comp-resolution frame covers (a region of interest, else the frame)…
    let (cw, ch) = (f64::from(comp.width), f64::from(comp.height));
    let clip = app.session.state.region_of_interest.unwrap_or([0.0, 0.0, cw, ch]);
    // …that the viewer shows.
    let a = map.to_comp(map.area.min);
    let b = map.to_comp(map.area.max);
    let snap_lo = |v: f64| (v * s / GRID).floor() * GRID / s;
    let snap_hi = |v: f64| (v * s / GRID).ceil() * GRID / s;
    let x0 = snap_lo(a[0]).max(clip[0]).max(0.0);
    let y0 = snap_lo(a[1]).max(clip[1]).max(0.0);
    let x1 = snap_hi(b[0]).min(clip[0] + clip[2]).min(cw);
    let y1 = snap_hi(b[1]).min(clip[1] + clip[3]).min(ch);
    let (w, h) = (x1 - x0, y1 - y0);
    if !(w > 0.0 && h > 0.0 && [x0, y0, w, h].iter().all(|v| v.is_finite())) {
        return None;
    }
    let side = crate::frames::max_texture_side(ctx) as f64;
    if (w * s).ceil() > side || (h * s).ceil() > side || w * h * s * s > MAX_TILE_PIXELS {
        return None;
    }
    Some((s, [x0, y0, w, h]))
}

impl SharpZoom {
    /// Take a finished render: its texture replaces the shown tile.
    fn collect(&mut self, ctx: &egui::Context) {
        let landed = match self.landed.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        let Some((key, image)) = landed else { return };
        if self.running == Some(key) {
            self.running = None;
        }
        if let Some(image) = image.filter(|i| i.size[0] > 0 && i.size[1] > 0) {
            let tex = ctx.load_texture("viewer-sharp-zoom", (*image).clone(), crate::panels::viewer::zoom_texture_options(false));
            self.shown = Some(Tile { key, image, tex });
        }
    }

    /// Start rendering `key` (nothing renders now).
    #[cfg(not(target_arch = "wasm32"))]
    fn start(&mut self, app_src: Source, ctx: &egui::Context, key: TileKey, opts: RenderOpts) {
        self.running = Some(key);
        let landed = self.landed.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("sharp-zoom".into()).spawn(move || {
            let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render_tile(&app_src.project, app_src.footage.as_ref(), app_src.expr.as_deref(), app_src.comp, app_src.time, opts)
            }))
            .ok()
            .map(Arc::new);
            if image.is_none() {
                log::warn!("sharp zoom: the tile render failed; the viewer shows the magnified frame");
            }
            match landed.lock() {
                Ok(mut g) => *g = Some((key, image)),
                Err(p) => *p.into_inner() = Some((key, image)),
            }
            ctx.request_repaint();
        });
        if spawned.is_err() {
            self.running = None;
        }
    }
}

/// What a tile render reads (shared snapshots, sent to its thread).
#[cfg(not(target_arch = "wasm32"))]
struct Source {
    project: Arc<effectcraft_engine::project::Project>,
    footage: Arc<dyn FootageSource>,
    expr: Option<Arc<dyn ExprHost>>,
    comp: ItemId,
    time: Tick,
}

/// The viewer's sharp tile: request the one the view wants and draw the newest of the same
/// frame over the magnified comp-resolution frame (`key` is that frame's key). Called after the
/// frame is drawn, before the overlays.
pub(crate) fn show(app: &mut EffectcraftApp, ctx: &egui::Context, painter: &egui::Painter, map: &ViewerMap, cid: ItemId, comp: &Comp, key: &FrameKey) {
    app.sharp_zoom.collect(ctx);
    let Some((scale, region)) = wanted(app, ctx, map, comp) else { return };
    let want = TileKey {
        comp: cid.0,
        content: key.content,
        frame: key.frame,
        view: key.view,
        opts: key.opts,
        scale: (scale * 1000.0).round() as u32,
        region,
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let sz = &app.sharp_zoom;
        if sz.running.is_none() && sz.shown.as_ref().is_none_or(|t| t.key != want) {
            let opts = sharp_opts(app.frame_opts(cid, 1.0), scale, region);
            let src = Source {
                project: app.session.project.clone(),
                footage: app.session.footage.clone(),
                expr: app.session.expr.clone(),
                comp: cid,
                time: comp.frame_rate.tick_of(key.frame),
            };
            app.sharp_zoom.start(src, ctx, want, opts);
        }
    }
    let Some(tile) = app.sharp_zoom.shown.as_ref().filter(|t| t.key.same_series(&want)) else { return };
    let r = tile_rect(map, &tile.key, tile.image.size);
    // Opaque over the magnified frame: the comp background (or the transparency grid) under it.
    if app.ui.viewer.transparency_grid {
        crate::panels::viewer::checker(painter, r);
    } else {
        let bg = comp.background;
        painter.rect_filled(r, 0.0, Color32::from_rgb((bg[0] * 255.0) as u8, (bg[1] * 255.0) as u8, (bg[2] * 255.0) as u8));
    }
    painter.image(tile.tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    app.auto.add("viewer.sharpZoom", r.intersect(map.area), "Sharp zoom tile");
}

/// Where a tile of `size` pixels lands on screen: the whole output pixels the renderer drew
/// (its region's corner rounded at the scale, as the region of interest is).
fn tile_rect(map: &ViewerMap, key: &TileKey, size: [usize; 2]) -> Rect {
    let s = key.scale();
    let (x, y) = ((key.region[0] * s).round() / s, (key.region[1] * s).round() / s);
    let min = map.to_screen([x, y]);
    Rect::from_min_size(min, vec2((size[0] as f64 / s) as f32 * map.zoom, (size[1] as f64 / s) as f32 * map.zoom))
}

impl EffectcraftApp {
    /// The sharp-zoom tile the viewer shows over the magnified frame, if any (tests, agents).
    pub fn sharp_zoom_tile(&self) -> Option<(TileKey, Arc<egui::ColorImage>)> {
        self.sharp_zoom.shown.as_ref().map(|t| (t.key, t.image.clone()))
    }

    /// A sharp-zoom tile is rendering.
    pub fn sharp_zoom_busy(&self) -> bool {
        self.sharp_zoom.running.is_some()
    }
}
