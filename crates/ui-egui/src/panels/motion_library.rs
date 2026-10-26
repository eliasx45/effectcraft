//! Motion Library panel (Window ▸ Motion Library): the card catalog kept in a remote bucket
//! (`effectcraft_motionlib`), on every machine the app runs on.
//!
//! - Folders on the left, a search field and the cards on the right. Thumbnails download as they
//!   scroll into view, a card's preview plays while the pointer is over it (Sound cards play
//!   their sound at the SFX preview volume, Display settings, through the app's audio output:
//!   [`crate::SoundHover`]), and applying a card
//!   (double-click or **Apply**) downloads its project and files, then nests its composition at
//!   the playhead (`library.apply`, one undo step).
//! - Cards whose footage stayed on the publishing Mac show **needs footage**: they import with
//!   missing footage items to relink (`footage.relink`).
//! - Opening the panel checks the remote; a newer catalog shows the **Library updated** chip, which
//!   switches to it (`library.update`).
//!
//! Everything here is also a command (`library.*`), so agents use the same library.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use effectcraft_engine::motion_library::LocalIndex;
use effectcraft_engine::motionlib::{BlobRef, BlobState, Card, Client, Crop, FolderRow};
use egui::{Align2, Color32, CornerRadius, Rect, Sense, Stroke, pos2, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::panel_kit as kit;
use crate::i18n::tr;
use crate::panels::timeline::TimelineHost;
use crate::theme::Tokens;
use crate::{EffectcraftApp, widgets};

/// What the panel shows (kept with the workspace; `motionLibrary.view` changes it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewState {
    pub query: String,
    /// Folder path (`""` = all).
    pub folder: String,
    /// The selected card's id.
    pub selected: Option<String>,
    /// Vertical scroll of the cards (points).
    pub scroll: f32,
    /// Interface scale in percent ([`SCALE_RANGE`], steps of 10).
    pub scale: u32,
    /// Card width in points at 100 % ([`CARD_SIZE_RANGE`]).
    pub card_size: f32,
    /// Folders open in the collection tree.
    pub expanded: BTreeSet<String>,
    /// Vertical scroll of the collection tree.
    pub folder_scroll: f32,
    /// The collections column is shown.
    pub show_folders: bool,
    /// The quick-action toolbar (comp tools) is shown.
    pub show_tools: bool,
    /// Only cards of this kind (`Comp`, `Overlay`, `Sound`…; empty = every kind).
    pub kind_filter: String,
    /// Only cards that can be applied here (not the ones applied on the Mac).
    pub here_only: bool,
    /// What the main area shows: `""` the cards, `curves` the Curves workspace, `tools` the
    /// Motion Library tools.
    pub workspace: String,
    /// The collection group shown (`""` = all collections).
    pub group: String,
    /// Card width per folder (the web panel remembers each folder's).
    pub folder_card_sizes: BTreeMap<String, f32>,
    /// Width of the collections column in points at 100 % (0 = automatic).
    pub sidebar_width: f32,
    /// SFX preview volume: how loud a Sound card plays while the pointer is over it (linear
    /// 0–1; the web panel's default is 20 %).
    pub sfx_volume: f32,
    /// The rail popover open: `display`, `filters`, `save` (empty = none).
    #[serde(skip)]
    pub popover: String,
    /// The card whose details are shown.
    #[serde(skip)]
    pub details: Option<String>,
    /// The details' Variants row is open (its saved variants with their situation cards).
    #[serde(skip)]
    pub variants_open: bool,
    /// Where the card being applied goes: comp seconds (a drop on the Timeline), else the playhead.
    #[serde(skip)]
    pub apply_time: Option<f64>,
    /// The card being applied (downloading) and when it started.
    #[serde(skip)]
    pub applying: Option<String>,
    /// The card being saved to the library and its job (`task:N`).
    #[serde(skip)]
    pub saving: Option<(String, String)>,
    /// A save waiting for a yes (its upload is over the size limit).
    #[serde(skip)]
    pub confirm_save: Option<SaveAsk>,
    /// A collection folder dialog (new, rename, delete).
    #[serde(skip)]
    pub folder_ask: Option<FolderAsk>,
    /// A card or group dialog (rename, describe, move, delete, groups).
    #[serde(skip)]
    pub card_ask: Option<CardAsk>,
    /// Save to Motion Library: the card's name and what to save (`layers`, `comp`, `preset`).
    #[serde(skip)]
    pub save_name: String,
    #[serde(skip)]
    pub save_kind: String,
    /// Save to Motion Library: its description, Comp's contents (`all` / `selected` layers) and
    /// Layers' card preview (`selected` layers only / `work-area`: the whole comp as a demo).
    #[serde(skip)]
    pub save_description: String,
    #[serde(skip)]
    pub save_contents: String,
    #[serde(skip)]
    pub save_preview: String,
}

/// A save to confirm: the card, the upload's size and what stays home.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SaveAsk {
    pub id: String,
    pub name: String,
    pub mb: f64,
    pub note: String,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            query: String::new(),
            folder: String::new(),
            selected: None,
            scroll: 0.0,
            scale: 100,
            card_size: DEFAULT_CARD_SIZE,
            expanded: BTreeSet::new(),
            folder_scroll: 0.0,
            show_folders: true,
            show_tools: true,
            kind_filter: String::new(),
            here_only: false,
            workspace: String::new(),
            group: String::new(),
            folder_card_sizes: BTreeMap::new(),
            sidebar_width: 0.0,
            sfx_volume: DEFAULT_SFX_VOLUME,
            popover: String::new(),
            details: None,
            variants_open: false,
            apply_time: None,
            applying: None,
            saving: None,
            confirm_save: None,
            folder_ask: None,
            card_ask: None,
            save_name: String::new(),
            save_kind: String::new(),
            save_description: String::new(),
            save_contents: String::new(),
            save_preview: String::new(),
        }
    }
}

/// Interface scale limits in percent (as the library's web panel).
pub const SCALE_RANGE: (u32, u32) = (80, 200);
/// Card width limits in points.
pub const CARD_SIZE_RANGE: (f32, f32) = (88.0, 1200.0);
const DEFAULT_CARD_SIZE: f32 = 144.0;
/// At or below this card width, names take two lines and the type line is hidden.
const COMPACT_CARD: f32 = 112.0;
/// SFX preview volume of a new panel (as the library's web panel).
pub const DEFAULT_SFX_VOLUME: f32 = 0.2;
/// A Sound card not on this machine plays on hover only when the bucket's file is at most this
/// big (a hover never downloads more).
const SOUND_HOVER_MAX_BYTES: u64 = 4 << 20;

impl ViewState {
    /// Keep settings in range (a hand-edited or old workspace).
    pub fn clamp(&mut self) {
        self.scale = (self.scale.clamp(SCALE_RANGE.0, SCALE_RANGE.1) / 10) * 10;
        self.card_size = if self.card_size.is_finite() { self.card_size.clamp(CARD_SIZE_RANGE.0, CARD_SIZE_RANGE.1) } else { DEFAULT_CARD_SIZE };
        if !self.scroll.is_finite() {
            self.scroll = 0.0;
        }
        if !self.folder_scroll.is_finite() {
            self.folder_scroll = 0.0;
        }
        self.sidebar_width = if self.sidebar_width.is_finite() && self.sidebar_width > 0.0 { self.sidebar_width.clamp(80.0, 420.0) } else { 0.0 };
        self.sfx_volume = if self.sfx_volume.is_finite() { self.sfx_volume.clamp(0.0, 1.0) } else { DEFAULT_SFX_VOLUME };
        self.folder_card_sizes.retain(|_, w| w.is_finite());
        for w in self.folder_card_sizes.values_mut() {
            *w = w.clamp(CARD_SIZE_RANGE.0, CARD_SIZE_RANGE.1);
        }
    }
    /// The view as `motionLibrary.view` reports it.
    pub fn to_json(&self) -> Value {
        json!({
            "folder": self.folder,
            "query": self.query,
            "selected": self.selected,
            "scale": self.scale,
            "cardSize": self.card_size,
            "sfxVolume": self.sfx_volume,
            "expanded": self.expanded,
            "display": self.popover == "display",
            "popover": self.popover,
            "showFolders": self.show_folders,
            "showTools": self.show_tools,
            "kind": self.kind_filter,
            "hereOnly": self.here_only,
            "workspace": self.workspace,
            "group": self.group,
            "details": self.details,
            "variants": self.variants_open,
        })
    }
    /// Open every folder above `folder` so its row shows in the tree.
    pub fn reveal(&mut self, folder: &str) {
        let mut path = String::new();
        let parts: Vec<&str> = folder.split('/').filter(|p| !p.is_empty()).collect();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            self.expanded.insert(path.clone());
        }
    }
}

/// Thumbnails kept as textures; the least recently shown go first.
const MAX_THUMBS: usize = 300;
/// Hover preview: frames per second decoded and at most this many (seconds × rate).
const PREVIEW_FPS: f64 = 12.0;
const PREVIEW_FRAMES: usize = 72;
const PREVIEW_SIDE: u32 = 320;

type Thumb = Option<(egui::TextureHandle, [u32; 2])>;
type ThumbPixels = Option<([u32; 2], Vec<u8>)>;
type ThumbJob = Arc<OnceLock<ThumbPixels>>;

static THUMB_JOBS: AtomicUsize = AtomicUsize::new(0);
const MAX_THUMB_JOBS: usize = 3;

/// Counts a running thumbnail decode (also when it ends early).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
struct ThumbSlot;

impl Drop for ThumbSlot {
    fn drop(&mut self) {
        THUMB_JOBS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Decode an image's bytes, cut to `crop` (its visible artwork) and shrink it to fit `side` ×
/// `side` (unpremultiplied RGBA).
fn decode_image(bytes: &[u8], side: u32, crop: Option<Crop>) -> ThumbPixels {
    let img = image::load_from_memory(bytes).ok()?;
    let img = match crop.and_then(|c| c.on(img.width(), img.height())) {
        Some([x, y, w, h]) => img.crop_imm(x, y, w, h),
        None => img,
    };
    let img = if img.width().max(img.height()) > side { img.thumbnail(side, side) } else { img };
    let rgba = img.to_rgba8();
    Some(([rgba.width(), rgba.height()], rgba.into_raw()))
}

/// A thumbnail's cache key: its file and the crop it shows.
fn thumb_key(file: &str, crop: Option<Crop>) -> String {
    match crop {
        Some(c) => format!("{file}#{},{},{},{}", c.x, c.y, c.width, c.height),
        None => file.to_string(),
    }
}

/// A thumbnail decoded before, or whose decode just finished: `Some(thumb)` once known (`None`
/// inside: it failed).
fn cached_thumb(ctx: &egui::Context, key: &str, side: u32) -> Option<Thumb> {
    let id = egui::Id::new(("ml-thumb", key, side));
    if let Some(t) = ctx.data(|d| d.get_temp::<Thumb>(id)) {
        crate::thumb_lru::touch::<Thumb>(ctx, "ml-thumb", id, MAX_THUMBS);
        return Some(t);
    }
    let jid = egui::Id::new(("ml-thumb-job", key, side));
    let job = ctx.data(|d| d.get_temp::<ThumbJob>(jid))?;
    let done = job.get()?;
    let thumb = done.as_ref().map(|([w, h], px)| {
        let img = egui::ColorImage::from_rgba_unmultiplied([*w as usize, *h as usize], px);
        (ctx.load_texture(format!("ml-{key}-{side}"), img, egui::TextureOptions::LINEAR), [*w, *h])
    });
    ctx.data_mut(|d| {
        d.insert_temp(id, thumb.clone());
        d.remove::<ThumbJob>(jid);
    });
    crate::thumb_lru::touch::<Thumb>(ctx, "ml-thumb", id, MAX_THUMBS);
    Some(thumb)
}

/// Remember that a thumbnail can't be shown.
fn failed_thumb(ctx: &egui::Context, key: &str, side: u32) {
    ctx.data_mut(|d| d.insert_temp::<Thumb>(egui::Id::new(("ml-thumb", key, side)), None));
}

/// Decode a thumbnail off the UI thread (`load` gives its bytes), a few at a time.
fn start_thumb(ctx: &egui::Context, key: &str, side: u32, crop: Option<Crop>, budget: &mut u32, load: impl FnOnce() -> Option<Vec<u8>> + Send + 'static) {
    let jid = egui::Id::new(("ml-thumb-job", key, side));
    if ctx.data(|d| d.get_temp::<ThumbJob>(jid)).is_some() || *budget == 0 || THUMB_JOBS.load(Ordering::Relaxed) >= MAX_THUMB_JOBS {
        return;
    }
    *budget -= 1;
    let job: ThumbJob = Arc::default();
    ctx.data_mut(|d| d.insert_temp(jid, job.clone()));
    #[cfg(target_arch = "wasm32")]
    {
        let _ = job.set(load().and_then(|b| decode_image(&b, side, crop)));
        ctx.request_repaint();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        THUMB_JOBS.fetch_add(1, Ordering::Relaxed);
        let slot = ThumbSlot;
        let ctx2 = ctx.clone();
        let spawned = std::thread::Builder::new().name("ec-library-thumb".into()).spawn(move || {
            let _slot = slot;
            let _ = job.set(load().and_then(|b| decode_image(&b, side, crop)));
            ctx2.request_repaint();
        });
        if spawned.is_err() {
            ctx.data_mut(|d| d.remove::<ThumbJob>(jid));
        }
    }
}

/// A card's thumbnail texture: downloaded (as it scrolls into view) and decoded off the UI thread.
fn thumbnail(ctx: &egui::Context, client: &Client, blob: &BlobRef, side: u32, crop: Option<Crop>, budget: &mut u32) -> Thumb {
    let key = thumb_key(&blob.hash, crop);
    if let Some(t) = cached_thumb(ctx, &key, side) {
        return t;
    }
    match client.blob(blob) {
        BlobState::Ready(_) => {}
        BlobState::Loading => return None,
        BlobState::Failed(_) => {
            failed_thumb(ctx, &key, side);
            return None;
        }
    }
    if *budget > 0 && THUMB_JOBS.load(Ordering::Relaxed) < MAX_THUMB_JOBS {
        let bytes = client.blob_bytes(blob)?;
        start_thumb(ctx, &key, side, crop, budget, move || Some(bytes));
    }
    None
}

/// A thumbnail from this Mac's library folder (cards the catalog doesn't list yet).
fn local_thumbnail(ctx: &egui::Context, path: &str, side: u32, crop: Option<Crop>, budget: &mut u32) -> Thumb {
    let key = thumb_key(path, crop);
    if let Some(t) = cached_thumb(ctx, &key, side) {
        return t;
    }
    let owned = path.to_string();
    start_thumb(ctx, &key, side, crop, budget, move || std::fs::read(owned).ok());
    None
}

/// The hover preview being decoded and played.
#[derive(Default)]
struct Preview {
    card: String,
    /// Decoded frames (size, RGBA), appended by the worker.
    frames: Arc<Mutex<Vec<([u32; 2], Vec<u8>)>>>,
    textures: Vec<egui::TextureHandle>,
    done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    started: Option<f64>,
    error: Option<String>,
}

type PreviewSlot = Arc<Mutex<Preview>>;

fn preview_slot(ctx: &egui::Context) -> PreviewSlot {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<PreviewSlot>(egui::Id::new("ml-preview")).clone())
}

/// Stop the hover preview (the pointer left the card).
fn stop_preview(ctx: &egui::Context) {
    let slot = preview_slot(ctx);
    let Ok(mut p) = slot.lock() else { return };
    if !p.card.is_empty() {
        p.cancel.store(true, Ordering::Relaxed);
        *p = Preview::default();
    }
}

/// What a card plays on hover from this machine's library folder, as the library's web panel
/// does: its preview movie, a video thumbnail, its file when that is a video (Overlay cards), else
/// its PNG frames. Read from disk: nothing downloads.
enum LocalPreview {
    Movie(String),
    Frames(Vec<String>),
}

/// PNG frames played on hover at most (seconds × [`PREVIEW_FPS`]).
const PREVIEW_PNG_FRAMES: usize = 120;

fn local_preview(local: &LocalIndex, card: &Card) -> Option<LocalPreview> {
    let e = local.get(&card.id)?;
    let is_file = |p: &str| !p.is_empty() && std::path::Path::new(p).is_file();
    let is_video = |p: &str| {
        let ext = std::path::Path::new(p).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        matches!(ext.as_str(), "mp4" | "mov" | "m4v" | "webm")
    };
    if is_file(&e.preview) {
        return Some(LocalPreview::Movie(e.preview.clone()));
    }
    for p in [&e.thumbnail, &e.file] {
        if is_video(p) && is_file(p) {
            return Some(LocalPreview::Movie(p.clone()));
        }
    }
    let frames = png_frames(&e.frames);
    (frames.len() >= 2).then_some(LocalPreview::Frames(frames))
}

/// A folder's PNGs in natural order (`frame2` before `frame10`), the first [`PREVIEW_PNG_FRAMES`].
fn png_frames(dir: &str) -> Vec<String> {
    if dir.is_empty() {
        return vec![];
    }
    let Ok(read) = std::fs::read_dir(dir) else { return vec![] };
    let mut paths: Vec<std::path::PathBuf> =
        read.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("png"))).collect();
    paths.sort_by_cached_key(|p| natural_key(&p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    paths.into_iter().take(PREVIEW_PNG_FRAMES).map(|p| p.to_string_lossy().to_string()).collect()
}

/// Sort key: runs of digits compare as numbers.
fn natural_key(name: &str) -> Vec<(u64, String)> {
    let mut out = vec![];
    let mut digits = String::new();
    let mut text = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !text.is_empty() {
                out.push((0, std::mem::take(&mut text)));
            }
            digits.push(c);
        } else {
            if !digits.is_empty() {
                out.push((digits.parse::<u64>().unwrap_or(u64::MAX).saturating_add(1), String::new()));
                digits.clear();
            }
            text.push(c.to_ascii_lowercase());
        }
    }
    if !digits.is_empty() {
        out.push((digits.parse::<u64>().unwrap_or(u64::MAX).saturating_add(1), String::new()));
    }
    if !text.is_empty() {
        out.push((0, text));
    }
    out
}

/// Decode PNG frames for the hover preview, appending them as they come.
fn decode_frames(paths: &[String], crop: Option<Crop>, frames: &Mutex<Vec<([u32; 2], Vec<u8>)>>, cancel: &AtomicBool, ctx: &egui::Context) {
    for path in paths {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some((size, px)) = std::fs::read(path).ok().and_then(|b| decode_image(&b, PREVIEW_SIDE, crop)) else { continue };
        if let Ok(mut v) = frames.lock() {
            v.push((size, px));
        }
        ctx.request_repaint();
    }
}

/// The preview frame to draw over `card` now (starting its decode on first hover: a local file,
/// else the preview downloaded once into the cache).
fn preview_frame(app: &dyn TimelineHost, ctx: &egui::Context, client: &Client, local: &LocalIndex, card: &Card) -> Option<egui::TextureHandle> {
    let slot = preview_slot(ctx);
    let mut p = slot.lock().ok()?;
    if p.card != card.id {
        p.cancel.store(true, Ordering::Relaxed);
        *p = Preview { card: card.id.clone(), ..Preview::default() };
    }
    if p.error.is_some() {
        return None;
    }
    if p.started.is_none() {
        // A card this machine has: its local files only (the bucket is for machines without them).
        let state = match local_preview(local, card) {
            Some(LocalPreview::Movie(path)) => BlobState::Ready(Some(std::path::PathBuf::from(path))),
            Some(LocalPreview::Frames(paths)) => {
                let (frames, done, cancel, ctx2, crop) = (p.frames.clone(), p.done.clone(), p.cancel.clone(), ctx.clone(), card.crop);
                let work = move || {
                    decode_frames(&paths, crop, &frames, &cancel, &ctx2);
                    done.store(true, Ordering::Relaxed);
                    ctx2.request_repaint();
                };
                #[cfg(target_arch = "wasm32")]
                work();
                #[cfg(not(target_arch = "wasm32"))]
                if std::thread::Builder::new().name("ec-library-preview".into()).spawn(work).is_err() {
                    p.error = Some("no thread for the preview".into());
                    return None;
                }
                p.started = Some(ctx.input(|i| i.time));
                return None;
            }
            None if is_local(card, local) => {
                p.error = Some("no preview on this Mac".into());
                return None;
            }
            None => client.blob(card.preview.as_ref()?),
        };
        match state {
            BlobState::Loading => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return None;
            }
            BlobState::Failed(e) => {
                p.error = Some(e);
                return None;
            }
            BlobState::Ready(path) => {
                let (Some(path), Some(imp)) = (path, app.session().importer.clone()) else {
                    p.error = Some("no video decoder here".into());
                    return None;
                };
                let footage = app.session().footage.clone();
                let (frames, done, cancel, ctx2, crop) = (p.frames.clone(), p.done.clone(), p.cancel.clone(), ctx.clone(), card.crop);
                let path = path.to_string_lossy().to_string();
                let work = move || {
                    decode_preview(&*imp, &*footage, &path, crop, &frames, &cancel, &ctx2);
                    done.store(true, Ordering::Relaxed);
                    ctx2.request_repaint();
                };
                #[cfg(target_arch = "wasm32")]
                work();
                #[cfg(not(target_arch = "wasm32"))]
                if std::thread::Builder::new().name("ec-library-preview".into()).spawn(work).is_err() {
                    p.error = Some("no thread for the preview".into());
                    return None;
                }
                p.started = Some(ctx.input(|i| i.time));
            }
        }
    }
    // New frames to textures.
    let fresh: Vec<([u32; 2], Vec<u8>)> = p.frames.lock().ok().map(|mut f| std::mem::take(&mut *f)).unwrap_or_default();
    for ([w, h], px) in fresh {
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
        let n = p.textures.len();
        p.textures.push(ctx.load_texture(format!("ml-preview-{}-{n}", card.id), img, egui::TextureOptions::LINEAR));
    }
    let n = p.textures.len();
    if n == 0 {
        if p.done.load(Ordering::Relaxed) {
            p.error = Some("the preview has no frames".into());
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        return None;
    }
    let now = ctx.input(|i| i.time);
    let k = ((now - p.started.unwrap_or(now)).max(0.0) * PREVIEW_FPS) as usize;
    let done = p.done.load(Ordering::Relaxed);
    let i = if done { k % n } else { k.min(n - 1) };
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(1.0 / PREVIEW_FPS));
    p.textures.get(i).cloned()
}

/// Decode a preview movie's first seconds at [`PREVIEW_FPS`], appending frames as they come.
fn decode_preview(
    imp: &dyn effectcraft_engine::Importer,
    footage: &dyn effectcraft_engine::render::FootageSource,
    path: &str,
    crop: Option<Crop>,
    frames: &Mutex<Vec<([u32; 2], Vec<u8>)>>,
    cancel: &AtomicBool,
    ctx: &egui::Context,
) {
    let Ok(f) = imp.probe(path) else { return };
    if !f.has_video {
        return;
    }
    let secs = f.duration.seconds();
    let count = ((secs * PREVIEW_FPS).floor() as usize).clamp(1, PREVIEW_FRAMES);
    let item = effectcraft_engine::project::ItemId(u64::MAX - 7);
    for k in 0..count {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let t = effectcraft_engine::time::Tick::from_seconds_f64(k as f64 / PREVIEW_FPS);
        let Some(img) = footage.frame(item, &f, t) else { break };
        // The card's visible artwork only (a transparent element in a big frame).
        if let Some([x, y, w, h]) = crop.and_then(|c| c.on(img.width, img.height))
            && let Some(full) = image::RgbaImage::from_raw(img.width, img.height, img.to_rgba8())
        {
            let cut = image::DynamicImage::ImageRgba8(full).crop_imm(x, y, w, h).thumbnail(PREVIEW_SIDE, PREVIEW_SIDE).to_rgba8();
            if let Ok(mut v) = frames.lock() {
                v.push(([cut.width(), cut.height()], cut.into_raw()));
            }
            ctx.request_repaint();
            continue;
        }
        let (w, h) = (img.width.max(1), img.height.max(1));
        let s = (PREVIEW_SIDE as f64 / w.max(h) as f64).min(1.0);
        let small = if s < 1.0 {
            effectcraft_engine::raster::resample(&img, ((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
        } else {
            (*img).clone()
        };
        if let Ok(mut v) = frames.lock() {
            v.push(([small.width, small.height], small.to_rgba8()));
        }
        ctx.request_repaint();
    }
    footage.forget(path);
}

/// A Sound card (SFX): it plays while the pointer is over it.
fn is_sound(card: &Card, local: &LocalIndex) -> bool {
    card.kind_label == "Sound" || local.get(&card.id).is_some_and(|e| e.kind == "audio")
}

fn is_audio_file(name: &str) -> bool {
    let ext = std::path::Path::new(name).extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "wav" | "mp3" | "m4a" | "aac" | "aif" | "aiff" | "flac" | "ogg" | "caf")
}

/// What a hovered Sound card can play.
enum SoundSource {
    File(String),
    /// The bucket's copy is downloading.
    Loading,
    None,
}

/// The file a Sound card plays on hover, as the web panel: this machine's library folder first
/// (its preview, else its file), else the bucket's copy once downloaded, only when it is at most
/// [`SOUND_HOVER_MAX_BYTES`] (a hover never downloads more).
fn sound_source(ctx: &egui::Context, client: &Client, local: &LocalIndex, card: &Card) -> SoundSource {
    if let Some(e) = local.get(&card.id) {
        for p in [&e.preview, &e.file] {
            if is_audio_file(p) && std::path::Path::new(p).is_file() {
                return SoundSource::File(p.clone());
            }
        }
    }
    let small = [card.preview.as_ref(), card.project.as_ref()].into_iter().flatten().find(|b| is_audio_file(&b.name) && b.size <= SOUND_HOVER_MAX_BYTES);
    let Some(blob) = small else { return SoundSource::None };
    match client.blob(blob) {
        BlobState::Ready(Some(path)) => SoundSource::File(path.to_string_lossy().to_string()),
        BlobState::Ready(None) | BlobState::Failed(_) => SoundSource::None,
        BlobState::Loading => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            SoundSource::Loading
        }
    }
}

/// Over a hovered Sound card: a speaker and the SFX volume while it plays, "Audio unavailable"
/// when it can't, nothing while the Timeline's own sound plays.
fn paint_sound_state(app: &mut dyn TimelineHost, p: &egui::Painter, thumb: Rect, card: &Card, src: &SoundSource, volume: f32, s: f32) {
    // The app's sound preview (a host without one shows no playing state).
    let sp = app.app_ref().and_then(|a| a.sound_preview.as_ref());
    let failed = matches!(src, SoundSource::None) || sp.and_then(|sp| sp.failed()).as_deref() == Some(card.id.as_str());
    let playing = sp.and_then(|sp| sp.playing()) == Some(card.id.as_str());
    let text = if matches!(src, SoundSource::Loading) {
        tr("Loading…").to_string()
    } else if failed {
        tr("Audio unavailable").to_string()
    } else if playing {
        format!("{}%", (volume * 100.0).round() as i32)
    } else {
        return;
    };
    let font = Tokens::medium(9.5 * s);
    let w = p.layout_no_wrap(text.clone(), font.clone(), Color32::WHITE).size().x;
    let icon = if playing && !failed { 14.0 * s } else { 0.0 };
    let pill = Rect::from_min_size(pos2(thumb.min.x + 5.0 * s, thumb.max.y - 21.0 * s), vec2(w + icon + 12.0 * s, 16.0 * s));
    p.rect_filled(pill, CornerRadius::same((8.0 * s) as u8), Color32::from_black_alpha(170));
    if icon > 0.0 {
        let ir = Rect::from_min_size(pill.min + vec2(5.0 * s, 2.0 * s), vec2(12.0 * s, 12.0 * s));
        crate::icons::paint(p, ir, crate::icons::Icon::Audio, Color32::WHITE);
    }
    let label = if playing && !failed { format!("Playing {text}") } else { text.clone() };
    p.text(pos2(pill.min.x + 6.0 * s + icon, pill.center().y), Align2::LEFT_CENTER, text, font, Color32::WHITE);
    app.auto_mut().add(&format!("motionLibrary.sound.{}", card.id), pill, &label);
}

/// This machine has the card's files (a library folder lists it): it applies from them, so it
/// isn't "on the Mac" and needs no footage here.
fn is_local(card: &Card, local: &LocalIndex) -> bool {
    local.get(&card.id).is_some_and(|e| !e.file.is_empty())
}

/// The card can be applied here (from the bucket or this machine's library folder).
fn usable(card: &Card, local: &LocalIndex) -> bool {
    card.applicable() || is_local(card, local)
}

/// How a card EffectCraft can't apply here is labelled: on a machine without the library's files
/// it's "on the Mac"; on the Mac that has them, it's a kind only After Effects uses (Layers,
/// presets, expressions…).
fn mac_words(local: &LocalIndex) -> (&'static str, &'static str) {
    if local.is_empty() {
        ("on the Mac", "On the Mac: apply it in After Effects there")
    } else {
        ("AE only", "After Effects only: EffectCraft can't use this kind yet; apply it from the Motion Library panel in After Effects")
    }
}

/// Listed only: applied in After Effects on the Mac that has its files.
fn on_mac(card: &Card, local: &LocalIndex) -> bool {
    card.on_mac && !is_local(card, local)
}

fn needs_footage(card: &Card, local: &LocalIndex) -> bool {
    card.needs_footage() && !is_local(card, local)
}

/// A card of this Mac's library folder whose files aren't in the bucket yet (listed only, or not
/// in the catalog at all): "Save to Library" uploads them.
fn unsynced(card: &Card, local: &LocalIndex) -> bool {
    is_local(card, local) && (card.on_mac || card.project.is_none())
}

/// Cards this Mac's library folder has that the catalog doesn't list yet (saved since the last
/// publish), shown with their local thumbnails.
fn local_only_cards(ctx: &egui::Context, client: &Client, local: &Arc<LocalIndex>) -> Arc<Vec<Card>> {
    type Cached = (u64, Arc<Vec<Card>>);
    let id = egui::Id::new("ml-local-only");
    let key = generation(client) ^ (Arc::as_ptr(local) as usize as u64).rotate_left(17);
    if let Some((k, cards)) = ctx.data(|d| d.get_temp::<Cached>(id))
        && k == key
    {
        return cards;
    }
    let cat = client.catalog();
    let listed: BTreeSet<&str> = cat.as_ref().map(|c| c.cards.iter().map(|c| c.id.as_str()).collect()).unwrap_or_default();
    let cards: Vec<Card> = local
        .values()
        .filter(|e| !listed.contains(e.id.as_str()))
        .map(|e| Card {
            id: e.id.clone(),
            name: e.name.clone(),
            category: e.category.clone(),
            kind: if e.kind == "comp" { "comp" } else { "footage" }.into(),
            kind_label: match e.kind.as_str() {
                "comp" => "Comp",
                "audio" => "Sound",
                "preset" => "Preset",
                _ => "Footage",
            }
            .into(),
            ..Card::default()
        })
        .collect();
    let cards = Arc::new(cards);
    ctx.data_mut(|d| d.insert_temp::<Cached>(id, (key, cards.clone())));
    cards
}

/// A card by id, as the panel lists it.
fn find_card(all: &[Card], id: &str) -> Option<Card> {
    all.iter().find(|c| c.id == id).cloned()
}

/// The adopted catalog's identity (a new one after an update).
fn generation(client: &Client) -> u64 {
    client.catalog().map_or(0, |c| Arc::as_ptr(&c) as usize as u64)
}

/// Cards matching the view (search, folder, kind, group), cached per card list and view. Browsing
/// everything (no folder, no query) groups them by folder in the tree's order.
fn visible_cards(
    ctx: &egui::Context,
    all: &Arc<Vec<Card>>,
    local: &Arc<LocalIndex>,
    lib: &LocalLib,
    tree: &Arc<Vec<FolderRow>>,
    view: &ViewState,
) -> Arc<Vec<Card>> {
    type Cached = ((usize, usize), String, String, String, bool, String, Arc<Vec<Card>>);
    let id = egui::Id::new("ml-cards");
    let key = (Arc::as_ptr(all) as usize, Arc::as_ptr(tree) as usize);
    if let Some((k, q, f, kind, h, g, cards)) = ctx.data(|d| d.get_temp::<Cached>(id))
        && k == key
        && q == view.query
        && f == view.folder
        && kind == view.kind_filter
        && h == view.here_only
        && g == view.group
    {
        return cards;
    }
    let ops = folder_ops();
    let mut cards: Vec<Card> = effectcraft_engine::motionlib::search(all, &view.query, &view.folder)
        .into_iter()
        .filter(|c| view.kind_filter.is_empty() || kind_label(c) == view.kind_filter)
        .filter(|c| !view.here_only || usable(c, local) || plug_use(c, local, lib).is_some())
        .filter(|c| view.group.is_empty() || folder_group(lib, &ops, &c.category) == view.group)
        .cloned()
        .collect();
    drop(ops);
    // This Mac's crops (the library folder's index) over the catalog's.
    for c in &mut cards {
        if let Some(crop) = local.get(&c.id).and_then(|e| e.crop) {
            c.crop = Some(crop);
        }
    }
    if grouped(view) {
        let ranks: BTreeMap<&str, usize> = tree.iter().enumerate().map(|(i, r)| (r.path.as_str(), i)).collect();
        cards.sort_by_key(|c| ranks.get(c.category.as_str()).copied().unwrap_or(usize::MAX));
    }
    let cards = Arc::new(cards);
    ctx.data_mut(|d| {
        d.insert_temp::<Cached>(id, (key, view.query.clone(), view.folder.clone(), view.kind_filter.clone(), view.here_only, view.group.clone(), cards.clone()))
    });
    cards
}

/// Cards show in sections per folder (browsing everything, no search).
fn grouped(view: &ViewState) -> bool {
    view.folder.is_empty() && view.query.trim().is_empty() && view.kind_filter.is_empty()
}

/// The collection tree (of collection group `group`; "" = all), cached per catalog, index, edit
/// and group: the published tree with this Mac's folders, order and edits, counting the cards
/// the panel lists.
fn folder_tree(ctx: &egui::Context, client: &Client, local: &Arc<LocalIndex>, lib: &Arc<LocalLib>, group: &str) -> Arc<Vec<FolderRow>> {
    type Cached = ((u64, u64, usize, usize, String), Arc<Vec<FolderRow>>);
    let id = egui::Id::new("ml-tree");
    let all = library_cards(ctx, client, local, lib);
    let key = (generation(client), folder_ops().revision, Arc::as_ptr(lib) as usize, Arc::as_ptr(&all) as usize, group.to_string());
    if let Some((k, rows)) = ctx.data(|d| d.get_temp::<Cached>(id))
        && k == key
    {
        return rows;
    }
    let mut catalog = client.catalog().map(|c| c.tree()).unwrap_or_default();
    let ops = folder_ops();
    // This Mac's complete index has every folder: a published one it lost was deleted here.
    if lib.complete {
        let kept: BTreeSet<&str> = lib.folders.iter().map(String::as_str).chain(all.iter().map(|c| c.category.as_str())).collect();
        catalog.retain(|r| kept.iter().any(|k| under(k, &r.path)));
    }
    let in_group = |f: &str| group.is_empty() || folder_group(lib, &ops, f) == group;
    let counts = folder_counts(all.iter().filter(|c| in_group(&c.category)));
    let mut card_folders: Vec<String> = all.iter().map(|c| c.category.clone()).filter(|c| !c.is_empty()).collect();
    card_folders.sort();
    card_folders.dedup();
    let sorting = lib.present().then(|| {
        let mut order = lib.order.clone();
        order.extend(ops.order.iter().map(|(k, v)| (k.clone(), *v)));
        Sorting { order, priorities: lib.priorities.clone() }
    });
    let mut rows = merge_folders(&catalog, &lib.folders, &ops, &card_folders, Some(&counts), sorting.as_ref());
    rows.retain(|r| in_group(&r.path));
    drop(ops);
    let rows = Arc::new(rows);
    ctx.data_mut(|d| d.insert_temp::<Cached>(id, (key, rows.clone())));
    rows
}

/// The rows of the tree whose parents are all open.
fn shown_rows<'a>(rows: &'a [FolderRow], expanded: &BTreeSet<String>) -> Vec<&'a FolderRow> {
    rows.iter()
        .filter(|r| {
            let mut path = r.path.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                if !expanded.contains(parent) {
                    return false;
                }
                path = parent;
            }
            true
        })
        .collect()
}

fn draw_text_clipped(p: &egui::Painter, r: Rect, text: &str, font: egui::FontId, color: Color32) {
    let galley = p.layout_no_wrap(text.to_string(), font, color);
    let pos = pos2(r.min.x, r.center().y - galley.size().y / 2.0);
    p.with_clip_rect(r.intersect(p.clip_rect())).galley(pos, galley, color);
}

/// `text` cut to `width` with an ellipsis.
fn elide(p: &egui::Painter, text: &str, font: &egui::FontId, width: f32) -> String {
    let fits = |s: &str| p.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x <= width;
    if fits(text) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars.iter().take(mid).collect::<String>() + "…";
        if fits(&s) { lo = mid } else { hi = mid - 1 }
    }
    chars.iter().take(lo).collect::<String>() + "…"
}

/// Where a card or section header sits in the scrolled content (x from the grid's left edge, y
/// from the top of the content).
enum Placed {
    Header { folder: String, count: usize, rect: Rect },
    Card { index: usize, rect: Rect, thumb_h: f32 },
}

struct Layout {
    items: Vec<Placed>,
    height: f32,
    col_w: f32,
}

/// The height of a card's thumbnail at `col_w` (its own shape; 16:10 without a thumbnail).
fn thumb_height(card: &Card, col_w: f32) -> f32 {
    let aspect = match card.crop.and_then(Crop::valid) {
        Some(c) => c.height as f32 / c.width as f32,
        None => card.thumb_size.filter(|[w, h]| *w > 0 && *h > 0).map_or(10.0 / 16.0, |[w, h]| h as f32 / w as f32),
    };
    (col_w * aspect.clamp(0.2, 2.5)).round()
}

/// Cards in columns of equal width: in folder sections, rows as tall as their tallest card
/// (`grouped`), else packed into the shortest column (search results, one folder).
fn layout(cards: &[Card], grouped: bool, width: f32, card_w: f32, gap: f32, caption_h: f32, header_h: f32) -> Layout {
    let cols = (((width + gap) / (card_w.min(width) + gap)).floor() as usize).max(1);
    let col_w = ((width - gap * (cols as f32 - 1.0)) / cols as f32).max(8.0);
    let mut items = Vec::with_capacity(cards.len() + 16);
    let mut y = 0.0f32;
    if grouped {
        let mut i = 0;
        while i < cards.len() {
            let folder = cards.get(i).map(|c| c.category.clone()).unwrap_or_default();
            let n = cards.iter().skip(i).take_while(|c| c.category == folder).count().max(1);
            items.push(Placed::Header { folder, count: n, rect: Rect::from_min_size(pos2(0.0, y), vec2(width, header_h)) });
            y += header_h + gap;
            for row_start in (i..i + n).step_by(cols) {
                let mut row_h = 0.0f32;
                for (col, index) in (row_start..(row_start + cols).min(i + n)).enumerate() {
                    let Some(card) = cards.get(index) else { break };
                    let th = thumb_height(card, col_w);
                    let h = th + caption_h;
                    row_h = row_h.max(h);
                    items.push(Placed::Card { index, rect: Rect::from_min_size(pos2(col as f32 * (col_w + gap), y), vec2(col_w, h)), thumb_h: th });
                }
                y += row_h + gap;
            }
            i += n;
        }
    } else {
        let mut heights = vec![0.0f32; cols];
        for (index, card) in cards.iter().enumerate() {
            let (col, top) = heights.iter().copied().enumerate().fold((0, f32::INFINITY), |best, (c, h)| if h < best.1 { (c, h) } else { best });
            let th = thumb_height(card, col_w);
            let h = th + caption_h;
            items.push(Placed::Card { index, rect: Rect::from_min_size(pos2(col as f32 * (col_w + gap), top), vec2(col_w, h)), thumb_h: th });
            if let Some(slot) = heights.get_mut(col) {
                *slot = top + h + gap;
            }
        }
        y = heights.iter().copied().fold(0.0, f32::max);
    }
    Layout { items, height: y, col_w }
}

/// A texture drawn to fill `r` (cropped to its shape, as CSS `object-fit: cover`).
fn paint_cover(p: &egui::Painter, tex: egui::TextureId, size: egui::Vec2, r: Rect) {
    let (tw, th) = (size.x.max(1.0), size.y.max(1.0));
    let (ra, ta) = (r.width() / r.height().max(1.0), tw / th);
    let uv = if ta > ra {
        let w = ra / ta;
        Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
    } else {
        let h = ta / ra;
        Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
    };
    p.image(tex, r, uv, Color32::WHITE);
}

/// Thumbnail decode size for a card `w` points wide (a few sizes, so zooming reuses textures).
fn thumb_side(w: f32, ppp: f32) -> u32 {
    match (w * ppp).ceil() as u32 {
        0..=160 => 160,
        161..=320 => 320,
        321..=640 => 640,
        _ => 1280,
    }
}

/// A folder's icon (its image, else the folder glyph).
fn paint_folder_icon(
    ctx: &egui::Context,
    client: &Client,
    p: &egui::Painter,
    row: Option<&FolderRow>,
    local_icon: Option<&str>,
    r: Rect,
    color: Color32,
    budget: &mut u32,
) {
    let Some(row) = row else {
        crate::icons::paint(p, r, crate::icons::Icon::Grid, color);
        return;
    };
    if let Some(path) = local_icon
        && let Some((tex, [w, h])) = local_thumbnail(ctx, path, 64, None, budget)
    {
        let s = (r.width() / w.max(1) as f32).min(r.height() / h.max(1) as f32);
        let ir = Rect::from_center_size(r.center(), vec2(w as f32, h as f32) * s);
        p.image(tex.id(), ir, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        return;
    }
    if let Some(blob) = &row.icon
        && let Some((tex, [w, h])) = thumbnail(ctx, client, blob, 64, None, budget)
    {
        let s = (r.width() / w.max(1) as f32).min(r.height() / h.max(1) as f32);
        let ir = Rect::from_center_size(r.center(), vec2(w as f32, h as f32) * s);
        p.image(tex.id(), ir, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        return;
    }
    crate::icons::paint(p, r, crate::icons::Icon::Folder, color);
}

/// A popover beside the rail button `anchor`: its frame, then `body` inside it. Closes on Escape
/// or a click outside it (the rail button toggles it itself).
fn popover(
    app: &mut dyn TimelineHost,
    ctx: &egui::Context,
    anchor: Rect,
    size: egui::Vec2,
    view: &mut ViewState,
    body: impl FnOnce(&mut dyn TimelineHost, &mut egui::Ui, Rect, &mut ViewState),
) {
    let t = app.tokens();
    let screen = ctx.content_rect();
    let pos = pos2(anchor.max.x + 6.0, anchor.min.y.min(screen.max.y - size.y - 4.0).max(screen.min.y + 4.0));
    let area = egui::Area::new(egui::Id::new(("ml-popover", view.popover.as_str()))).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
        let (r, _) = ui.allocate_exact_size(size, Sense::click());
        ui.painter().rect_filled(r, CornerRadius::same(8), t.header_bg);
        ui.painter().rect_stroke(r, CornerRadius::same(8), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
        body(app, ui, r, view);
        r
    });
    app.auto_mut().add(&format!("motionLibrary.popover.{}", view.popover), area.inner, &view.popover);
    let outside = ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.inner.contains(p) && !anchor.contains(p)));
    if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        view.popover.clear();
    }
}

/// Display settings: interface scale, card size, SFX preview volume.
fn display_body(app: &mut dyn TimelineHost, ui: &mut egui::Ui, r: Rect, view: &mut ViewState, s: f32) {
    let t = app.tokens();
    let pad = 12.0 * s;
    ui.painter().text(pos2(r.min.x + pad, r.min.y + 16.0 * s), Align2::LEFT_CENTER, tr("Display settings"), Tokens::semibold(12.0 * s), t.text);
    // Interface scale: − 100% +.
    let y1 = r.min.y + 44.0 * s;
    ui.painter().text(pos2(r.min.x + pad, y1), Align2::LEFT_CENTER, tr("Interface scale"), Tokens::ui(11.5 * s), t.text_dim);
    let bw = 24.0 * s;
    let plus = Rect::from_center_size(pos2(r.max.x - pad - bw / 2.0, y1), vec2(bw, 22.0 * s));
    let value = Rect::from_center_size(pos2(plus.min.x - 26.0 * s, y1), vec2(44.0 * s, 22.0 * s));
    let minus = Rect::from_center_size(pos2(value.min.x - bw / 2.0 - 2.0 * s, y1), vec2(bw, 22.0 * s));
    if kit::button(app, ui, minus, "motionLibrary.scaleDown", "−", false) && view.scale > SCALE_RANGE.0 {
        view.scale -= 10;
    }
    if kit::button(app, ui, plus, "motionLibrary.scaleUp", "+", false) && view.scale < SCALE_RANGE.1 {
        view.scale += 10;
    }
    let pct = format!("{}%", view.scale);
    ui.painter().text(value.center(), Align2::CENTER_CENTER, &pct, Tokens::medium(11.5 * s), t.text);
    app.auto_mut().add("motionLibrary.scale", value, &pct);
    // Card size: a slider.
    let y2 = r.min.y + 78.0 * s;
    ui.painter().text(pos2(r.min.x + pad, y2), Align2::LEFT_CENTER, tr("Card size"), Tokens::ui(11.5 * s), t.text_dim);
    let track = Rect::from_min_max(pos2(r.min.x + pad + 76.0 * s, y2 - 8.0 * s), pos2(r.max.x - pad, y2 + 8.0 * s));
    let resp = ui.interact(track, egui::Id::new("ml-card-size"), Sense::click_and_drag());
    let (lo, hi) = CARD_SIZE_RANGE;
    // Each folder keeps its own card size (All motion: the default), as on the web panel.
    let mut size = view.folder_card_sizes.get(&view.folder).copied().unwrap_or(view.card_size);
    if (resp.dragged() || resp.clicked())
        && let Some(x) = resp.interact_pointer_pos().map(|p| p.x)
    {
        let f = ((x - track.min.x) / track.width().max(1.0)).clamp(0.0, 1.0);
        size = (lo + f * (hi - lo)).round();
        if view.folder.is_empty() {
            view.card_size = size;
        } else {
            view.folder_card_sizes.insert(view.folder.clone(), size);
        }
    }
    let f = ((size - lo) / (hi - lo)).clamp(0.0, 1.0);
    let line = Rect::from_min_max(pos2(track.min.x, y2 - 1.5 * s), pos2(track.max.x, y2 + 1.5 * s));
    let p = ui.painter();
    p.rect_filled(line, CornerRadius::same(2), t.field_border);
    p.rect_filled(Rect::from_min_max(line.min, pos2(track.min.x + f * track.width(), line.max.y)), CornerRadius::same(2), t.accent);
    p.circle_filled(pos2(track.min.x + f * track.width(), y2), 6.0 * s, if resp.hovered() || resp.dragged() { t.accent_hover } else { t.accent });
    app.auto_mut().add_response("motionLibrary.cardSize", &resp, &format!("{size}"));
    // SFX preview volume: a slider, 0–100 %.
    let y3 = r.min.y + 112.0 * s;
    let pct = format!("{}%", (view.sfx_volume * 100.0).round() as i32);
    ui.painter().text(pos2(r.min.x + pad, y3), Align2::LEFT_CENTER, tr("SFX volume"), Tokens::ui(11.5 * s), t.text_dim);
    let vw = 34.0 * s;
    let track = Rect::from_min_max(pos2(r.min.x + pad + 76.0 * s, y3 - 8.0 * s), pos2(r.max.x - pad - vw, y3 + 8.0 * s));
    let resp = ui.interact(track, egui::Id::new("ml-sfx-volume"), Sense::click_and_drag());
    if (resp.dragged() || resp.clicked())
        && let Some(x) = resp.interact_pointer_pos().map(|p| p.x)
    {
        view.sfx_volume = (((x - track.min.x) / track.width().max(1.0)).clamp(0.0, 1.0) * 100.0).round() / 100.0;
    }
    let f = view.sfx_volume.clamp(0.0, 1.0);
    let line = Rect::from_min_max(pos2(track.min.x, y3 - 1.5 * s), pos2(track.max.x, y3 + 1.5 * s));
    let p = ui.painter();
    p.rect_filled(line, CornerRadius::same(2), t.field_border);
    p.rect_filled(Rect::from_min_max(line.min, pos2(track.min.x + f * track.width(), line.max.y)), CornerRadius::same(2), t.accent);
    p.circle_filled(pos2(track.min.x + f * track.width(), y3), 6.0 * s, if resp.hovered() || resp.dragged() { t.accent_hover } else { t.accent });
    p.text(pos2(r.max.x - pad, y3), Align2::RIGHT_CENTER, &pct, Tokens::medium(11.0 * s), t.text);
    app.auto_mut().add_response(
        "motionLibrary.sfxVolume",
        &resp.on_hover_text(tr("SFX preview volume: how loud a Sound card plays while the pointer is over it")),
        &pct,
    );
}

/// Filters: what kind of card (Comp, Overlay, Sound…) and only cards that apply here.
fn filters_body(app: &mut dyn TimelineHost, ui: &mut egui::Ui, r: Rect, view: &mut ViewState, kinds: &[(String, usize)], s: f32) {
    let t = app.tokens();
    ui.painter().text(pos2(r.min.x + 12.0 * s, r.min.y + 16.0 * s), Align2::LEFT_CENTER, tr("Filter cards"), Tokens::semibold(12.0 * s), t.text);
    let mut y = r.min.y + 30.0 * s;
    let total: usize = kinds.iter().map(|(_, n)| n).sum();
    let all = [(String::new(), total)];
    for (kind, n) in all.iter().chain(kinds.iter()) {
        let row = Rect::from_min_size(pos2(r.min.x + 6.0 * s, y), vec2(r.width() - 12.0 * s, 22.0 * s));
        let id = if kind.is_empty() { "all".to_string() } else { kind.clone() };
        let resp = ui.interact(row, egui::Id::new(("ml-filter", &id)), Sense::click());
        let active = view.kind_filter == *kind;
        if active {
            ui.painter().rect_filled(row, CornerRadius::same(4), t.accent.gamma_multiply(0.28));
        } else if resp.hovered() {
            ui.painter().rect_filled(row, CornerRadius::same(4), t.hover);
        }
        let name = if kind.is_empty() { tr("All kinds").to_string() } else { kind.clone() };
        ui.painter().text(pos2(row.min.x + 8.0 * s, row.center().y), Align2::LEFT_CENTER, &name, Tokens::ui(11.5 * s), t.text);
        ui.painter().text(pos2(row.max.x - 8.0 * s, row.center().y), Align2::RIGHT_CENTER, n.to_string(), Tokens::ui(10.5 * s), t.text_faint);
        app.auto_mut().add(&format!("motionLibrary.filter.{id}"), row, &name);
        if resp.clicked() {
            view.kind_filter = kind.clone();
            view.scroll = 0.0;
        }
        y += 23.0 * s;
    }
    let at = pos2(r.min.x + 12.0 * s, y + 6.0 * s);
    let on = kit::checkbox(app, ui, at, "motionLibrary.filter.here", tr("Only cards that apply here"), view.here_only);
    if on != view.here_only {
        view.here_only = on;
        view.scroll = 0.0;
    }
}

/// What "Save to Motion Library" can save from the open project.
const SAVE_KINDS: [(&str, &str, &str); 4] = [
    ("layers", "Layers", "The selected layers (with their parents and mattes), editable"),
    ("comp", "Comp", "The open composition (or its selected layers) and everything it uses"),
    ("preset", "Preset", "The selected properties or effects (values, keyframes, expressions), to apply to other layers"),
    ("expression", "Expression", "The selected property's expression (works in After Effects too)"),
];

/// Save to Motion Library: a name, what to save (selected layers, the comp, a preset, an
/// expression), a description, Comp's contents or Layers' preview, and the collection open in the
/// tree; the card lands in the local library with its preview.
fn save_body(app: &mut dyn TimelineHost, ui: &mut egui::Ui, r: Rect, view: &mut ViewState, s: f32) -> Option<(&'static str, Value)> {
    let t = app.tokens();
    let p = ui.painter().clone();
    p.text(pos2(r.min.x + 12.0 * s, r.min.y + 16.0 * s), Align2::LEFT_CENTER, tr("Save to Motion Library"), Tokens::semibold(12.0 * s), t.text);
    let x0 = r.min.x + 12.0 * s;
    let w = r.width() - 24.0 * s;
    let field = Rect::from_min_size(pos2(x0, r.min.y + 32.0 * s), vec2(w, 24.0 * s));
    let resp = ui.put(field, egui::TextEdit::singleline(&mut view.save_name).hint_text(tr("Card name")));
    app.auto_mut().add_response("motionLibrary.save.name", &resp, "Card name");
    if view.save_kind.is_empty() {
        view.save_kind = "layers".into();
    }
    let n = SAVE_KINDS.len() as f32;
    let bw = (w - 4.0 * s * (n - 1.0)) / n;
    let kinds_y = field.max.y + 8.0 * s;
    for (i, (key, label, tip)) in SAVE_KINDS.iter().enumerate() {
        let b = Rect::from_min_size(pos2(x0 + i as f32 * (bw + 4.0 * s), kinds_y), vec2(bw, 24.0 * s));
        if kit::button(app, ui, b, &format!("motionLibrary.save.kind.{key}"), label, view.save_kind == *key) {
            view.save_kind = key.to_string();
        }
        ui.interact(b, egui::Id::new(("ml-save-tip", key)), Sense::hover()).on_hover_text(tr(tip));
    }
    let mut y = kinds_y + 32.0 * s;
    // Comp: the whole comp or its selected layers; Layers: what the card's preview shows.
    let choice: Option<(&str, [(&str, &str); 2])> = match view.save_kind.as_str() {
        "comp" => Some(("contents", [("all", "Entire composition"), ("selected", "Selected layers")])),
        "layers" => Some(("preview", [("selected", "Preview: selected layers only"), ("work-area", "Preview: full comp demo")])),
        _ => None,
    };
    if let Some((which, options)) = choice {
        let slot = if which == "contents" { &mut view.save_contents } else { &mut view.save_preview };
        let cur = options.iter().position(|(k, _)| *k == slot.as_str()).unwrap_or(0);
        let labels: Vec<&str> = options.iter().map(|(_, l)| *l).collect();
        let dr = Rect::from_min_size(pos2(x0, y), vec2(w, 22.0 * s));
        let picked = kit::dropdown(app, ui, dr, &format!("motionLibrary.save.{which}"), &labels, cur);
        if let Some((k, _)) = picked.and_then(|i| options.get(i)) {
            let slot = if which == "contents" { &mut view.save_contents } else { &mut view.save_preview };
            *slot = k.to_string();
        }
        y += 30.0 * s;
    }
    let desc = Rect::from_min_size(pos2(x0, y), vec2(w, 44.0 * s));
    let dresp = ui.put(desc, egui::TextEdit::multiline(&mut view.save_description).hint_text(tr("Description (optional): when to use it, what it does")));
    app.auto_mut().add_response("motionLibrary.save.description", &dresp, "Description");
    y = desc.max.y + 12.0 * s;
    let folder = if view.folder.is_empty() { "My motion".to_string() } else { view.folder.clone() };
    let into = format!("{} {folder}", tr("Into:"));
    let shown = elide(&p, &into, &Tokens::ui(11.0 * s), w - 100.0 * s);
    p.text(pos2(x0, y + 12.0 * s), Align2::LEFT_CENTER, &shown, Tokens::ui(11.0 * s), t.text_dim);
    let save = Rect::from_min_size(pos2(r.max.x - 12.0 * s - 90.0 * s, y), vec2(90.0 * s, 24.0 * s));
    let name = view.save_name.trim().to_string();
    let enter = resp.lost_focus() && ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
    if (kit::button(app, ui, save, "motionLibrary.save.go", "Save", true) || enter) && !name.is_empty() {
        view.popover.clear();
        view.save_name.clear();
        let description = std::mem::take(&mut view.save_description).trim().to_string();
        let mut params = json!({"kind": view.save_kind, "name": name, "category": folder, "description": description});
        match view.save_kind.as_str() {
            "comp" => params["contents"] = json!(if view.save_contents.is_empty() { "all" } else { view.save_contents.as_str() }),
            "layers" => params["preview"] = json!(if view.save_preview.is_empty() { "selected" } else { view.save_preview.as_str() }),
            _ => {}
        }
        return Some(("motionLibrary.saveSelection", params));
    }
    None
}

/// The Save popover's height for what it shows.
fn save_height(view: &ViewState) -> f32 {
    if matches!(view.save_kind.as_str(), "comp" | "layers" | "") { 236.0 } else { 206.0 }
}

/// Wait (up to 2 min) for a new card's preview, which renders in a job of its own, so the
/// rebuilt index has its preview and poster.
fn wait_for_preview(id: &str) {
    if id.is_empty() || id.contains(['/', '.']) {
        return;
    }
    let root = std::env::var_os("ELIAS_MOTION_LIBRARY")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Documents/Elias Motion Library")));
    let Some(dir) = root.map(|r| r.join("previews").join(id)) else { return };
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(120) {
        if dir.join("preview.mp4").is_file() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// A card's name as the panel shows it (renamed here, or only on this Mac).
pub fn card_name(app: &mut dyn crate::panels::timeline::TimelineHost, ctx: &egui::Context, id: &str) -> Option<String> {
    let session = app.session();
    let client = session.motion_library().ok()?;
    let local = session.motion_library_local();
    let lib = local_library(ctx, &session.prefs.libraries);
    find_card(&library_cards(ctx, &client, &local, &lib), id).map(|c| c.name)
}

/// `motionLibrary.saveSelection`: save the selected layers (`kind` layers; `preview` selected or
/// work-area), the open comp (comp; `contents` all or selected), the selected properties' values
/// and animation (preset) or the selected property's expression (expression) as a card `name`
/// (with an optional `description`) in collection `category` of the local library (the Motion Library's EffectCraft plug, which reads the
/// project through this app's control port). It runs in the background; the status line says how
/// it went and the card shows once the library index is rebuilt.
pub fn save_selection_command(app: &mut dyn TimelineHost, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    if name.is_empty() {
        return Err("missing `name` (the card's name)".into());
    }
    let category = params.get("category").and_then(Value::as_str).map(|c| c.trim_matches('/')).filter(|c| !c.is_empty()).unwrap_or("My motion").to_string();
    let kind = params.get("kind").and_then(Value::as_str).unwrap_or("layers");
    let description = params.get("description").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    let pick = |key: &str, options: &[&str]| -> Result<String, String> {
        let v = params.get(key).and_then(Value::as_str).unwrap_or(options.first().copied().unwrap_or_default());
        if options.contains(&v) { Ok(v.to_string()) } else { Err(format!("`{key}` is one of {}", options.join(", "))) }
    };
    let (op, args): (&'static str, Value) = match kind {
        "layers" => (
            "save_layers",
            json!({"name": name, "category": category, "description": description, "preview_mode": pick("preview", &["selected", "work-area"])?}),
        ),
        "comp" => {
            ("save", json!({"name": name, "category": category, "description": description, "composition_selection": pick("contents", &["all", "selected"])?}))
        }
        "preset" => ("save_asset", json!({"kind": "preset", "name": name, "category": category, "description": description})),
        "expression" => ("save_asset", json!({"kind": "expression", "name": name, "category": category, "description": description})),
        other => return Err(format!("unknown kind `{other}` (layers, comp, preset or expression)")),
    };
    let port = control_port(app)?;
    app.ui_mut().status = format!("Saving “{name}” to the Motion Library…");
    let done = format!("Saved “{name}” to {category}");
    let save = move || {
        let r = ml_rpc("effectcraft_plug.py", &[], Some(port), op, args)?;
        wait_for_preview(r.get("id").and_then(Value::as_str).unwrap_or_default());
        Ok(r)
    };
    spawn_ml_op(ctx, save, FolderChange::Created(category.clone()), done);
    Ok(json!({"saving": name, "category": category, "kind": kind}))
}

/// This app's control port: Motion Library operations on the open project (saving, tools,
/// curves) reach it through the Motion Library's EffectCraft plug.
pub(crate) fn control_port(app: &dyn TimelineHost) -> Result<u16, String> {
    // A host serving the plug its own project (EMBED13: FigCraft's card Timeline).
    if let Some(port) = app.library_port() {
        return Ok(port);
    }
    let scripting = &app.session().prefs.scripting;
    std::env::var("EFFECTCRAFT_CONTROL_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .or_else(|| scripting.agent_control.then(|| u16::try_from(scripting.agent_control_port).ok()).flatten())
        .ok_or_else(|| "this reads the project through the control channel: turn on Settings ▸ Scripting ▸ Let Agents Control EffectCraft".to_string())
}

/// One host operation of the Motion Library's EffectCraft plug (`effectcraft_plug.py`: curves,
/// tools, saving) on the open project, through this app's control port. Blocks: run it off the UI
/// thread ([`spawn_ml_op`]).
pub(crate) fn plug_rpc(port: u16, op: &str, args: Value) -> Result<Value, String> {
    ml_rpc("effectcraft_plug.py", &[], Some(port), op, args)
}

/// The quick actions on the right: comp tools on the open composition's selection.
const TOOLS: [(&str, crate::icons::Icon, &str, &str); 7] = [
    ("smartPrecomp", crate::icons::Icon::NewComp, "Smart Precomp: the selected layers into one precomp trimmed to their span", "layer.smartPrecompose"),
    ("decomp", crate::icons::Icon::Collapse, "Decomp: replace the selected precomp layer by its layers", "layer.decompose"),
    ("multiPrecomp", crate::icons::Icon::Comp, "Multi Precomp: each selected layer into its own precomp", "layer.multiPrecompose"),
    ("deepDuplicate", crate::icons::Icon::Link, "Deep Duplicate: copy the open comp and every comp nested in it", "project.deepDuplicate"),
    ("splitMasks", crate::icons::Icon::MaskVis, "Split Masks: one layer per mask of the selected layer", "layer.splitMasks"),
    ("gradientLock", crate::icons::Icon::Lock, "Gradient Lock: shape gradients follow their rectangle, ellipse or polystar", "shape.gradientLock"),
    ("easyEase", crate::icons::Icon::Keyframe, "Easy Ease the selected keyframes (F9)", "keys.easyEase"),
];

/// What a comp tool did, for the status bar.
fn tool_message(name: &str, r: &Value) -> String {
    let n = |k: &str| r.get(k).and_then(Value::as_array).map_or(0, Vec::len);
    match name {
        "multiPrecomp" => format!("Multi Precomp: {} precomps", n("precomps")),
        "decomp" => format!("Decomp: {} layers expanded (the comp stays in the project)", r["expanded"]),
        "deepDuplicate" => format!("Deep Duplicate: {} compositions copied", n("items")),
        "splitMasks" => format!("Split Masks: {} layers", n("layers")),
        "gradientLock" => format!("Gradient Lock: {} locked, {} already", r["locked"], r["already"]),
        "smartPrecomp" => format!("Smart Precomp: {}", r["name"].as_str().unwrap_or("done")),
        _ => "Done".into(),
    }
}

fn toolbar(app: &mut dyn TimelineHost, ui: &mut egui::Ui, bar: Rect, s: f32) {
    let t = app.tokens();
    ui.painter().rect_filled(bar, 0.0, t.panel_bg);
    ui.painter().line_segment([bar.left_top(), bar.left_bottom()], Stroke::new(1.0, t.separator));
    let side = (28.0 * s).round();
    let mut y = bar.min.y + 8.0 * s;
    for (name, icon, tip, cmd) in TOOLS {
        let r = Rect::from_min_size(pos2(bar.center().x - side / 2.0, y), vec2(side, side));
        if r.max.y > bar.max.y {
            break;
        }
        let resp = widgets::icon_button(ui, r, icon, false, &t, egui::Id::new(("ml-tool", name)));
        app.auto_mut().add(&format!("motionLibrary.tool.{name}"), r, tip);
        if resp.on_hover_text(tr(tip)).clicked()
            && let Some(v) = kit::exec(app, cmd, json!({}))
        {
            app.ui_mut().status = tool_message(name, &v);
        }
        y += side + 6.0 * s;
    }
}

/// The left rail: library views and filters, the label strip, sync state, settings and save.
#[allow(clippy::too_many_arguments)]
fn rail(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    client: &Client,
    status: &effectcraft_engine::motionlib::Status,
    bar: Rect,
    view: &mut ViewState,
    s: f32,
) {
    let t = app.tokens();
    ui.painter().rect_filled(bar, 0.0, t.header_bg);
    ui.painter().line_segment([bar.right_top(), bar.right_bottom()], Stroke::new(1.0, t.separator));
    let side = (28.0 * s).round();
    let x = bar.center().x - side / 2.0;
    let mut y = bar.min.y + 8.0 * s;
    let mut anchors: Vec<(&str, Rect)> = vec![];
    use crate::icons::Icon;
    let top: [(&str, Icon, &str, bool); 10] = [
        (
            "assets",
            Icon::Grid,
            "Assets: everything in the library",
            view.workspace.is_empty() && view.folder.is_empty() && view.kind_filter.is_empty() && view.query.is_empty(),
        ),
        ("curves", Icon::Graph, "Curves: the graph editor and curve presets", view.workspace == "curves"),
        ("tools", Icon::Sparkle, "Tools: Workflow, Text Split, Bounce, Colour", view.workspace == "tools"),
        // Batch 1 lane C (CX2): ⌘V link paste's tab.
        // CX3: style families (change one, all follow).
        ("families", Icon::TextLayer, "Families: style families and Text animations", view.workspace == "families"),
        // CX7: what you did (the learning loop reads it too).
        ("history", Icon::Chat, "History: what you did, for you and for Claude", view.workspace == "history"),
        ("link", Icon::Link, "Link: paste an Instagram link onto a layer", view.workspace == "link"),
        ("search", Icon::Search, "Search assets", false),
        ("filters", Icon::Hamburger, "Filter cards by kind", view.popover == "filters" || !view.kind_filter.is_empty() || view.here_only),
        ("folders", Icon::Folder, "Show or hide the collections", view.show_folders),
        ("sfx", Icon::Audio, "SFX: the sound effects collection", view.folder == "SFX" || view.folder.starts_with("SFX/")),
    ];
    for (name, icon, tip, active) in top {
        let r = Rect::from_min_size(pos2(x, y), vec2(side, side));
        let resp = widgets::icon_button(ui, r, icon, active, &t, egui::Id::new(("ml-rail", name)));
        app.auto_mut().add(&format!("motionLibrary.rail.{name}"), r, tip);
        if resp.on_hover_text(tr(tip)).clicked() {
            match name {
                "assets" => {
                    view.workspace.clear();
                    view.folder.clear();
                    view.kind_filter.clear();
                    view.query.clear();
                    view.scroll = 0.0;
                }
                "curves" | "tools" | "link" | "families" | "history" => {
                    view.workspace = if view.workspace == name { String::new() } else { name.to_string() };
                    view.popover.clear();
                }
                "search" => {
                    view.workspace.clear();
                    ctx.data_mut(|d| d.insert_temp(egui::Id::new("ml-focus-search"), true));
                }
                "folders" => view.show_folders = !view.show_folders,
                "sfx" => {
                    if client.catalog().is_some_and(|c| c.cards.iter().any(|c| c.in_folder("SFX"))) {
                        view.folder = "SFX".into();
                        view.reveal("SFX");
                        view.scroll = 0.0;
                    } else {
                        app.ui_mut().status = "This library has no SFX collection".into();
                    }
                }
                other => {
                    view.popover = if view.popover == other { String::new() } else { other.to_string() };
                }
            }
        }
        anchors.push((name, r));
        y += side + 4.0 * s;
    }
    // Tools: the quick actions on the right.
    let tr_r = Rect::from_min_size(pos2(bar.min.x + 2.0, y), vec2(bar.width() - 4.0, 18.0 * s));
    let tresp = ui.interact(tr_r, egui::Id::new("ml-rail-tools"), Sense::click());
    ui.painter().text(tr_r.center(), Align2::CENTER_CENTER, tr("Tools"), Tokens::medium(9.5 * s), if view.show_tools { t.text } else { t.text_faint });
    app.auto_mut().add("motionLibrary.rail.tools", tr_r, "Tools");
    if tresp.on_hover_text(tr("Show or hide the quick actions")).clicked() {
        view.show_tools = !view.show_tools;
    }
    y += 24.0 * s;
    // Bottom: sync state, refresh, settings, save.
    let bottom: [(&str, Option<Icon>, &str); 4] = [
        ("ec", None, "Library state (click to check for updates)"),
        ("refresh", None, "Refresh: check for library updates and sync the library's sources"),
        ("display", Some(Icon::Gear), "Display settings"),
        ("save", Some(Icon::Plus), "Save to Motion Library"),
    ];
    let mut by = bar.max.y - 8.0 * s - side;
    let bottom_top = bar.max.y - 8.0 * s - 4.0 * (side + 4.0 * s);
    for (name, icon, tip) in bottom.iter().rev() {
        let r = Rect::from_min_size(pos2(x, by), vec2(side, side));
        by -= side + 4.0 * s;
        if r.min.y < y {
            continue;
        }
        let resp = match (name, icon) {
            (&"save", Some(i)) => {
                let resp = ui.interact(r, egui::Id::new(("ml-rail", *name)), Sense::click());
                ui.painter().circle_filled(r.center(), side / 2.0, if resp.hovered() { t.accent_hover } else { t.accent });
                crate::icons::paint(ui.painter(), r.shrink(7.0 * s), *i, Color32::WHITE);
                resp
            }
            (_, Some(i)) => widgets::icon_button(ui, r, *i, view.popover == *name, &t, egui::Id::new(("ml-rail", *name))),
            (&"ec", None) => {
                let resp = ui.interact(r, egui::Id::new(("ml-rail", *name)), Sense::click());
                ui.painter().text(r.center(), Align2::CENTER_CENTER, "EC", Tokens::semibold(10.0 * s), t.text_dim);
                let dot = match (&status.error, &status.update) {
                    (Some(_), _) => t.danger,
                    (None, Some(_)) => t.warning,
                    _ if status.refreshing || !status.checked => t.text_faint,
                    _ => t.cache_green,
                };
                ui.painter().circle_filled(pos2(r.max.x - 5.0 * s, r.min.y + 6.0 * s), 3.0 * s, dot);
                resp
            }
            _ => {
                let resp = ui.interact(r, egui::Id::new(("ml-rail", *name)), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(4), t.hover);
                }
                ui.painter().text(r.center(), Align2::CENTER_CENTER, if status.refreshing { "…" } else { "↻" }, Tokens::ui(15.0 * s), t.text_dim);
                resp
            }
        };
        let auto = if *name == "refresh" || *name == "display" { format!("motionLibrary.{name}") } else { format!("motionLibrary.rail.{name}") };
        let label = match (*name, &status.update, &status.error) {
            ("ec", _, Some(e)) => e.clone(),
            ("ec", Some(u), None) => u.label.clone(),
            ("ec", None, None) => format!("version {}", status.version),
            _ => tip.to_string(),
        };
        app.auto_mut().add(&auto, r, &label);
        if resp.on_hover_text(label).clicked() {
            match *name {
                "ec" => {
                    kit::exec(app, "library.sync", json!({"wait": false}));
                }
                "refresh" => {
                    app.ui_mut().motion_library = view.clone();
                    if let Err(e) = app.invoke(ctx, "motionLibrary.refresh", json!({})) {
                        app.ui_mut().status = e;
                    }
                    *view = app.ui_mut().motion_library.clone();
                }
                other => view.popover = if view.popover == other { String::new() } else { other.to_string() },
            }
        }
        anchors.push((name, r));
    }
    // The label strip: label the selected layers.
    let sw = (10.0 * s).round();
    let cols = 2usize;
    let strip_w = cols as f32 * sw + 3.0 * s;
    let mut sy = y + 4.0 * s;
    for (i, l) in effectcraft_color::Label::ALL.iter().skip(1).enumerate() {
        let (col, row) = (i % cols, i / cols);
        let r = Rect::from_min_size(pos2(bar.center().x - strip_w / 2.0 + col as f32 * (sw + 3.0 * s), sy + row as f32 * (sw + 3.0 * s)), vec2(sw, sw));
        if r.max.y > bottom_top - 4.0 * s {
            break;
        }
        let resp = ui.interact(r, egui::Id::new(("ml-label", i)), Sense::click());
        ui.painter().rect_filled(r, CornerRadius::same(2), t.label(*l));
        if resp.hovered() {
            ui.painter().rect_stroke(r.expand(1.0), CornerRadius::same(2), Stroke::new(1.0, t.text), egui::StrokeKind::Outside);
        }
        let name = app.session().prefs.label_name(*l);
        app.auto_mut().add(&format!("motionLibrary.label.{}", l.name()), r, &name);
        if resp.on_hover_text(format!("Label the selected layers: {name}")).clicked() {
            kit::exec(app, "edit.label", json!({"label": l.name()}));
        }
    }
    sy += 0.0;
    let _ = sy;
    // Popovers beside their buttons.
    let kinds = kind_counts(ctx, client);
    let which = view.popover.clone();
    if let Some((_, anchor)) = anchors.iter().find(|(n, _)| *n == which) {
        let anchor = *anchor;
        match which.as_str() {
            "display" => popover(app, ctx, anchor, vec2(236.0 * s, 138.0 * s), view, |app, ui, r, view| display_body(app, ui, r, view, s)),
            "filters" => {
                let h = (66.0 + 23.0 * (kinds.len() as f32 + 1.0)) * s;
                popover(app, ctx, anchor, vec2(240.0 * s, h), view, |app, ui, r, view| filters_body(app, ui, r, view, &kinds, s))
            }
            "save" => {
                let mut pick = None;
                let h = save_height(view) * s;
                popover(app, ctx, anchor, vec2(300.0 * s, h), view, |app, ui, r, view| pick = save_body(app, ui, r, view, s));
                if let Some((cmd, params)) = pick {
                    app.ui_mut().motion_library = view.clone();
                    if let Err(e) = app.invoke(ctx, cmd, params) {
                        app.ui_mut().status = e;
                    }
                    *view = app.ui_mut().motion_library.clone();
                }
            }
            _ => {}
        }
    }
}

/// How many cards of each kind (Comp, Overlay, Sound…) the catalog has, most first.
fn kind_counts(ctx: &egui::Context, client: &Client) -> Arc<Vec<(String, usize)>> {
    type Cached = (u64, Arc<Vec<(String, usize)>>);
    let id = egui::Id::new("ml-kinds");
    let generation = generation(client);
    if let Some((g, k)) = ctx.data(|d| d.get_temp::<Cached>(id))
        && g == generation
    {
        return k;
    }
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for c in client.catalog().iter().flat_map(|c| c.cards.iter()) {
        *counts.entry(kind_label(c).to_string()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let v = Arc::new(v);
    ctx.data_mut(|d| d.insert_temp::<Cached>(id, (generation, v.clone())));
    v
}

/// The card's kind as the library labels it.
fn kind_label(c: &Card) -> &str {
    if !c.kind_label.is_empty() {
        &c.kind_label
    } else if c.kind == "comp" {
        "Comp"
    } else {
        "Footage"
    }
}

/// Whether "Save Selection as Variant" can run: layers selected in the open comp and a library
/// folder to keep the variant in.
fn can_variant(app: &dyn TimelineHost) -> bool {
    let s = app.session();
    !s.prefs.libraries.is_empty() && s.active_comp().is_some() && !s.state.selected_layers.is_empty()
}

/// A card's right-click menu: each entry is a command (with an automation id) → the one picked.
fn card_menu(
    auto: &mut crate::automation::Registry,
    resp: &egui::Response,
    card: &Card,
    local: &LocalIndex,
    lib: &LocalLib,
    can_save: bool,
    can_variant: bool,
    danger: Color32,
) -> Option<(&'static str, Value)> {
    let mut pick = None;
    crate::widgets::context_menu(resp, |ui| pick = card_menu_entries(ui, auto, card, local, lib, can_save, can_variant, danger, "menu"));
    pick
}

/// The card's menu entries (its right-click menu, the details' ••• menu); `prefix`: their
/// automation ids' (`motionLibrary.<prefix>.<entry>`).
#[allow(clippy::too_many_arguments)]
fn card_menu_entries(
    ui: &mut egui::Ui,
    auto: &mut crate::automation::Registry,
    card: &Card,
    local: &LocalIndex,
    lib: &LocalLib,
    can_save: bool,
    can_variant: bool,
    danger: Color32,
    prefix: &str,
) -> Option<(&'static str, Value)> {
    let mut pick = None;
    let here = usable(card, local);
    let plug = plug_use(card, local, lib);
    let has_file = is_local(card, local);
    let save = unsynced(card, local) && can_save;
    // This Mac keeps the library: its cards can be edited.
    let edit = lib.cards.get(&card.id).filter(|_| ml_backend().is_some_and(|p| p.is_file()));
    let id = card.id.as_str();
    let mut entry = |ui: &mut egui::Ui, key: &str, label: &str, enabled: bool, cmd: &'static str, params: Value, color: Option<Color32>| {
        let text = egui::RichText::new(tr(label));
        let b = ui.add_enabled(
            enabled,
            egui::Button::new(match color {
                Some(c) => text.color(c),
                None => text,
            }),
        );
        auto.add_response(&format!("motionLibrary.{prefix}.{key}"), &b, label);
        if b.clicked() {
            pick = Some((cmd, params));
            ui.close();
        }
    };
    match plug {
        Some(how) => {
            let label = match how {
                "ease" => "Apply Ease to Selected Keyframes",
                "expression" => "Apply Expression to Selected Property",
                _ => "Insert Comp (Converts the AE Project)",
            };
            entry(ui, "apply", label, true, "motionLibrary.card", json!({"op": "use", "id": id}), None);
        }
        None => {
            entry(ui, "apply", "Apply at Playhead", here, "motionLibrary.apply", json!({"id": id}), None);
            entry(ui, "applyAtStart", "Apply at Composition Start", here, "motionLibrary.apply", json!({"id": id, "time": 0.0}), None);
        }
    }
    ui.separator();
    if prefix == "menu" {
        entry(ui, "details", "Details…", true, "motionLibrary.card", json!({"op": "details", "id": id}), None);
    }
    entry(
        ui,
        "showInCollection",
        "Show in Collection",
        true,
        "motionLibrary.view",
        json!({"folder": card.category, "query": "", "kind": "", "select": id, "details": null}),
        None,
    );
    entry(ui, "reveal", "Reveal in Finder", has_file, "library.reveal", json!({"id": id}), None);
    entry(ui, "copyName", "Copy Name", true, "motionLibrary.copyName", json!({"id": id}), None);
    // The refined layers (or precomp) selected in the open comp, remembered with their situation.
    ui.separator();
    entry(ui, "saveVariant", "Save Selection as Variant", can_variant, "library.saveVariant", json!({"parent": id}), None);
    if let Some(e) = edit {
        ui.separator();
        entry(ui, "rename", "Rename…", true, "motionLibrary.card", json!({"op": "rename", "id": id}), None);
        entry(ui, "describe", "Edit Description…", true, "motionLibrary.card", json!({"op": "describe", "id": id}), None);
        entry(ui, "move", "Move to Folder…", true, "motionLibrary.card", json!({"op": "move", "id": id}), None);
        entry(ui, "thumbnail", "Choose Thumbnail…", true, "motionLibrary.card", json!({"op": "setThumbnail", "id": id}), None);
        if e.thumbnail_override || folder_ops().card_thumbs.get(id).is_some_and(Option::is_some) {
            entry(ui, "resetThumbnail", "Restore Default Thumbnail", true, "motionLibrary.card", json!({"op": "resetThumbnail", "id": id}), None);
        }
        if e.app == "effectcraft" {
            entry(ui, "preview", "Generate Preview", true, "motionLibrary.card", json!({"op": "preview", "id": id}), None);
        }
        if e.kind_label == "Ease" {
            ui.separator();
            entry(ui, "editGraph", "Edit Graph…", true, "motionLibrary.card", json!({"op": "editGraph", "id": id}), None);
            entry(ui, "duplicateGraph", "Duplicate Graph", true, "motionLibrary.card", json!({"op": "duplicateGraph", "id": id}), None);
            entry(ui, "reverseGraph", "Reverse Graph", true, "motionLibrary.card", json!({"op": "reverseGraph", "id": id}), None);
        }
    }
    if has_file {
        ui.separator();
        entry(ui, "save", if unsynced(card, local) { "Save to Library" } else { "Saved to Library" }, save, "motionLibrary.save", json!({"id": id}), None);
    }
    if edit.is_some() {
        ui.separator();
        entry(ui, "delete", "Delete Card…", true, "motionLibrary.card", json!({"op": "delete", "id": id}), Some(danger));
    }
    pick
}

/// A folder being dragged in the collection tree.
#[derive(Clone, Debug)]
struct FolderDrag(String);

/// What is dropped on `resp`, when it's a `P`. egui's `dnd_release_payload` takes the payload
/// whatever its type (a card dropped on a folder row was lost to the row's folder-drop check),
/// so the type is checked first and anything else is left for the next check.
fn dropped<P: std::any::Any + Send + Sync>(resp: &egui::Response) -> Option<Arc<P>> {
    if egui::DragAndDrop::has_payload_of_type::<P>(&resp.ctx) { resp.dnd_release_payload::<P>() } else { None }
}

/// Where a folder dropped on a row goes: before or after it (a sibling), or inside it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DropPlace {
    Before,
    Inside,
    After,
}

/// The row's top or bottom edge reorders, its middle (and "All motion") takes the folder in.
fn drop_place(path: &str, row: Rect, pointer: Option<egui::Pos2>) -> DropPlace {
    let Some(q) = pointer.filter(|_| !path.is_empty()) else { return DropPlace::Inside };
    let f = (q.y - row.min.y) / row.height().max(1.0);
    if f < 0.3 {
        DropPlace::Before
    } else if f > 0.7 {
        DropPlace::After
    } else {
        DropPlace::Inside
    }
}

/// Where a folder dropped at `place` on `target` lands, given the row shown `above` it. The line
/// between the last row of a group and the next, shallower row is drawn in one spot for "after the
/// group's last folder" and "before the next row": for a folder from that group it stays a
/// reorder among its siblings (it moved the folder out of its parent).
fn boundary_drop(folder: &str, target: &str, place: DropPlace, above: Option<&str>) -> (String, DropPlace) {
    let parent = |p: &str| p.rsplit_once('/').map_or(String::new(), |(a, _)| a.to_string());
    let depth = |p: &str| p.matches('/').count();
    if let (DropPlace::Before, Some(above)) = (place, above)
        && parent(target) != parent(folder)
        && depth(above) > depth(target)
    {
        let mut q = above;
        loop {
            if parent(q) == parent(folder) {
                if q != folder {
                    return (q.to_string(), DropPlace::After);
                }
                break;
            }
            match q.rsplit_once('/') {
                Some((up, _)) => q = up,
                None => break,
            }
        }
    }
    (target.to_string(), place)
}

/// The command a folder dropped on `target` (`""`: All motion) runs.
fn folder_drop(folder: &str, target: &str, place: DropPlace) -> (&'static str, Value) {
    let parent = |p: &str| p.rsplit_once('/').map_or(String::new(), |(a, _)| a.to_string());
    match place {
        DropPlace::Inside => ("motionLibrary.folder", json!({"op": "move", "category": folder, "parent": target})),
        _ if parent(folder) == parent(target) => {
            ("motionLibrary.folder", json!({"op": "reorder", "category": folder, "target": target, "after": place == DropPlace::After}))
        }
        _ => ("motionLibrary.folder", json!({"op": "move", "category": folder, "parent": parent(target)})),
    }
}

/// The collection groups popover: show all collections or one group; new, rename, delete.
fn groups_body(app: &mut dyn TimelineHost, ui: &mut egui::Ui, r: Rect, view: &ViewState, groups: &[String], s: f32) -> Option<(&'static str, Value)> {
    let t = app.tokens();
    ui.painter().text(pos2(r.min.x + 12.0 * s, r.min.y + 16.0 * s), Align2::LEFT_CENTER, tr("Collection groups"), Tokens::semibold(12.0 * s), t.text);
    let mut y = r.min.y + 30.0 * s;
    let mut pick = None;
    let all = [String::new()];
    for g in all.iter().chain(groups.iter()) {
        let row = Rect::from_min_size(pos2(r.min.x + 6.0 * s, y), vec2(r.width() - 12.0 * s, 22.0 * s));
        let resp = ui.interact(row, egui::Id::new(("ml-group", g)), Sense::click());
        if view.group == *g {
            ui.painter().rect_filled(row, CornerRadius::same(4), t.accent.gamma_multiply(0.28));
        } else if resp.hovered() {
            ui.painter().rect_filled(row, CornerRadius::same(4), t.hover);
        }
        let name = if g.is_empty() { tr("All collections").to_string() } else { g.clone() };
        ui.painter().text(pos2(row.min.x + 8.0 * s, row.center().y), Align2::LEFT_CENTER, &name, Tokens::ui(11.5 * s), t.text);
        app.auto_mut().add(&format!("motionLibrary.group.{}", if g.is_empty() { "all" } else { g }), row, &name);
        if resp.clicked() {
            pick = Some(("motionLibrary.group", json!({"op": "choose", "name": g})));
        }
        y += 24.0 * s;
    }
    ui.painter().line_segment([pos2(r.min.x + 8.0 * s, y + 3.0 * s), pos2(r.max.x - 8.0 * s, y + 3.0 * s)], Stroke::new(1.0, t.separator));
    y += 8.0 * s;
    let row_button = |app: &mut dyn TimelineHost, ui: &mut egui::Ui, y: f32, key: &str, label: &str| {
        let b = Rect::from_min_size(pos2(r.min.x + 6.0 * s, y), vec2(r.width() - 12.0 * s, 22.0 * s));
        kit::button(app, ui, b, &format!("motionLibrary.group.{key}"), label, false)
    };
    if row_button(app, ui, y, "new", "New group…") {
        pick = Some(("motionLibrary.group", json!({"op": "create"})));
    }
    if !view.group.is_empty() {
        if row_button(app, ui, y + 24.0 * s, "rename", "Rename group…") {
            pick = Some(("motionLibrary.group", json!({"op": "rename", "name": view.group})));
        }
        if row_button(app, ui, y + 48.0 * s, "delete", "Delete group…") {
            pick = Some(("motionLibrary.group", json!({"op": "delete", "name": view.group})));
        }
    }
    pick
}

/// What a save leaves at home (its job's `skipped`), as a sentence.
fn skipped_note(result: &Value) -> String {
    let skipped = result["skipped"].as_array().cloned().unwrap_or_default();
    if skipped.is_empty() {
        return String::new();
    }
    let names: Vec<String> =
        skipped.iter().take(4).map(|k| format!("{} ({})", k["file"].as_str().unwrap_or_default(), k["reason"].as_str().unwrap_or_default())).collect();
    let more = if skipped.len() > 4 { format!(" and {} more", skipped.len() - 4) } else { String::new() };
    format!("Left at home (relink on other machines): {}{more}.", names.join(", "))
}

/// Ask before a big upload: `Some(true)` upload, `Some(false)` cancel.
fn confirm_save_sheet(app: &mut dyn TimelineHost, ui: &mut egui::Ui, grid: Rect, ask: &SaveAsk, s: f32) -> Option<bool> {
    let t = app.tokens();
    let p = ui.painter().with_clip_rect(grid);
    let _ = ui.interact(grid, egui::Id::new("ml-confirm-block"), Sense::click_and_drag());
    p.rect_filled(grid, 0.0, Color32::from_black_alpha(120));
    let w = (grid.width() - 32.0 * s).clamp(120.0, 420.0 * s);
    let h = (grid.height() - 32.0 * s).clamp(90.0, 190.0 * s);
    let r = Rect::from_center_size(grid.center(), vec2(w, h));
    p.rect_filled(r, CornerRadius::same(10), t.header_bg);
    p.rect_stroke(r, CornerRadius::same(10), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    app.auto_mut().add("motionLibrary.confirmSave", r, &ask.name);
    let inner = r.shrink(14.0 * s);
    let title = format!("Upload {:.1} MB to the library?", ask.mb);
    p.text(inner.min, Align2::LEFT_TOP, &title, Tokens::semibold(13.0 * s), t.text);
    let body = format!("Saving “{}” uploads its files over your internet connection. {}", ask.name, ask.note);
    let text_r = Rect::from_min_max(pos2(inner.min.x, inner.min.y + 24.0 * s), pos2(inner.max.x, inner.max.y - 32.0 * s));
    let galley = p.layout(body, Tokens::ui(11.5 * s), t.text_dim, text_r.width().max(40.0));
    p.with_clip_rect(text_r.intersect(grid)).galley(text_r.min, galley, t.text_dim);
    let bw = 84.0 * s;
    let up = Rect::from_min_size(pos2(inner.max.x - bw, inner.max.y - 24.0 * s), vec2(bw, 24.0 * s));
    let cancel = up.translate(vec2(-(bw + 8.0 * s), 0.0));
    if kit::button(app, ui, cancel, "motionLibrary.confirmSave.cancel", "Cancel", false) || ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
        return Some(false);
    }
    if kit::button(app, ui, up, "motionLibrary.confirmSave.upload", "Upload", true) {
        return Some(true);
    }
    None
}

/// A small cloud with an up arrow (not synced: save to the library).
fn paint_cloud_up(p: &egui::Painter, r: Rect, color: Color32) {
    let u = r.height();
    let base = r.center().y + u * 0.18;
    let stroke = Stroke::new((u * 0.11).max(1.0), color);
    let (l, rr) = (r.center().x - u * 0.55, r.center().x + u * 0.55);
    p.circle_stroke(pos2(r.center().x - u * 0.22, base - u * 0.12), u * 0.25, stroke);
    p.circle_stroke(pos2(r.center().x + u * 0.12, base - u * 0.22), u * 0.32, stroke);
    p.line_segment([pos2(l + u * 0.1, base + u * 0.08), pos2(rr - u * 0.05, base + u * 0.08)], stroke);
    let x = r.center().x;
    p.line_segment([pos2(x, base + u * 0.02), pos2(x, base - u * 0.42)], stroke);
    p.line_segment([pos2(x - u * 0.17, base - u * 0.25), pos2(x, base - u * 0.42)], stroke);
    p.line_segment([pos2(x + u * 0.17, base - u * 0.25), pos2(x, base - u * 0.42)], stroke);
}

/// The card's details over the grid: preview image (an Ease card's speed graph), what and where
/// it is, its description, what the library knows of it (`describe`), its files, Apply and its
/// menu. Returns the card to apply (`.0`, applied from files) and the command applying it through
/// the plug (`.1`).
#[allow(clippy::too_many_arguments)]
fn details_sheet(
    app: &mut dyn TimelineHost,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    client: &Client,
    grid: Rect,
    card: &Card,
    lib: &LocalLib,
    plug: Option<&'static str>,
    view: &mut ViewState,
    s: f32,
) -> Option<(String, (&'static str, Value))> {
    let t = app.tokens();
    let p = ui.painter().with_clip_rect(grid);
    // Block the cards below.
    let _ = ui.interact(grid, egui::Id::new("ml-details-block"), Sense::click_and_drag());
    p.rect_filled(grid, 0.0, Color32::from_black_alpha(120));
    let w = (grid.width() - 32.0 * s).min(560.0 * s).max(120.0);
    let h = (grid.height() - 32.0 * s).min(520.0 * s).max(120.0);
    let r = Rect::from_center_size(grid.center(), vec2(w, h));
    p.rect_filled(r, CornerRadius::same(10), t.header_bg);
    p.rect_stroke(r, CornerRadius::same(10), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    app.auto_mut().add("motionLibrary.details", r, &card.name);
    let pad = 14.0 * s;
    let inner = r.shrink(pad);
    let edit = lib.cards.get(&card.id);
    let entry = edit.and_then(|_| described(ctx, &card.id)).and_then(Result::ok);
    // Image: the thumbnail at its shape (an Ease card's speed graph), up to 40 % of the height.
    let max_h = inner.height() * 0.4;
    let aspect = card.thumb_size.filter(|[a, b]| *a > 0 && *b > 0).map_or(10.0 / 16.0, |[a, b]| b as f32 / a as f32);
    let (iw, ih) = if inner.width() * aspect > max_h { (max_h / aspect, max_h) } else { (inner.width(), inner.width() * aspect) };
    let img = Rect::from_min_size(pos2(inner.center().x - iw / 2.0, inner.min.y), vec2(iw, ih));
    p.rect_filled(img, CornerRadius::same(6), t.field_bg);
    let ease = edit.filter(|e| !e.thumbnail_override).and_then(|e| e.speed_graph.as_ref()).and_then(keys_curve);
    let mut budget = 2u32;
    if let Some(curve) = ease {
        paint_speed_graph(&p, img, curve, t.accent, t.separator);
    } else if let Some((tex, [tw, th])) =
        card.thumbnail.as_ref().and_then(|b| thumbnail(ctx, client, b, thumb_side(iw, ctx.pixels_per_point()), card.crop, &mut budget))
    {
        paint_cover(&p, tex.id(), vec2(tw as f32, th as f32), img);
    }
    let mut y = img.max.y + 14.0 * s;
    let name_font = Tokens::semibold(14.0 * s);
    let name = elide(&p, &card.name, &name_font, inner.width() - 30.0 * s);
    p.text(pos2(inner.min.x, y), Align2::LEFT_TOP, &name, name_font, t.text);
    // ••• : the card's menu.
    let more = Rect::from_min_size(pos2(inner.max.x - 24.0 * s, y - 2.0 * s), vec2(24.0 * s, 20.0 * s));
    let mresp = ui.interact(more, egui::Id::new("ml-details-more"), Sense::click());
    p.rect_filled(more, CornerRadius::same(4), if mresp.hovered() { t.hover } else { Color32::TRANSPARENT });
    p.text(more.center(), Align2::CENTER_CENTER, "•••", Tokens::semibold(11.0 * s), t.text_dim);
    app.auto_mut().add_response("motionLibrary.details.more", &mresp, "More actions");
    let mut menu_pick = None;
    let local = app.session().motion_library_local();
    let saving = view.saving.is_none();
    let can_v = can_variant(app);
    egui::Popup::menu(&mresp).style(widgets::menu_style).show(|ui| {
        menu_pick = card_menu_entries(ui, app.auto_mut(), card, &local, lib, saving, can_v, t.danger, "details");
    });
    y += 20.0 * s;
    p.text(pos2(inner.min.x, y), Align2::LEFT_TOP, card.subtitle(), Tokens::ui(11.0 * s), t.text_faint);
    y += 20.0 * s;
    let mut facts = vec![];
    if let Some(d) = card.duration.filter(|d| *d > 0.0) {
        facts.push(format!("{d:.1} s"));
    }
    if let Some(e) = &entry {
        if let Some(fps) = e["fps"].as_f64().filter(|f| *f > 0.0) {
            facts.push(format!("{fps} fps"));
        }
        if let Some(v) = e["versions"]["current_version"].as_u64().filter(|v| *v > 1) {
            facts.push(format!("version {v}"));
        }
        if let Some(st) = e["index_status"].as_str() {
            facts.push(tr(index_label(st)).to_string());
        }
        match e["preview_status"].as_str() {
            Some("queued") => facts.push(tr("preview queued").to_string()),
            Some("rendering") => facts.push(tr("creating the preview…").to_string()),
            Some("failed") => facts.push(tr("the preview couldn't be made: Generate Preview tries again").to_string()),
            _ => {}
        }
    }
    if let Some(e) = local.get(&card.id).filter(|_| is_local(card, &local)) {
        facts.push(format!("from this Mac's library: {}", e.file));
        if unsynced(card, &local) {
            facts.push(tr("not synced: right-click ▸ Save to Library uploads it").to_string());
        }
    } else if let Some(how) = plug {
        facts.push(
            tr(match how {
                "ease" => "applies to the selected keyframes (two adjacent keys on one property)",
                "expression" => "applies to the selected property",
                _ => "converts its After Effects project, then nests the comp",
            })
            .to_string(),
        );
    } else if card.on_mac {
        facts.push(mac_words(&local).1.to_string());
    } else {
        let bytes: u64 = card.apply_blobs().iter().map(|b| b.size).sum();
        facts.push(format!(
            "{} file{} · {:.1} MB to download",
            card.apply_blobs().len(),
            if card.apply_blobs().len() == 1 { "" } else { "s" },
            bytes as f64 / 1e6
        ));
    }
    if needs_footage(card, &local) {
        let names: Vec<&str> = card.placeholders.iter().map(|p| p.name.as_str()).take(4).collect();
        facts.push(format!("needs footage: {}", names.join(", ")));
    }
    let buttons_h = 34.0 * s;
    let mut text_rect = Rect::from_min_max(pos2(inner.min.x, y), pos2(inner.max.x, inner.max.y - buttons_h));
    // Variants: refined copies saved with the situation each was made for; the row opens a list.
    let saved = effectcraft_engine::library_variants::index(app.session()).by_parent.get(&card.id).cloned().unwrap_or_default();
    if !saved.is_empty() {
        let (head_h, row_h) = (22.0 * s, 36.0 * s);
        // Room for the rows, keeping a few lines of the description.
        let room = ((text_rect.height() - head_h - 48.0 * s) / row_h).floor().max(0.0) as usize;
        let shown = if view.variants_open { saved.len().min(4).min(room) } else { 0 };
        let more = view.variants_open && saved.len() > shown;
        let area_h = head_h + shown as f32 * row_h + if more { 16.0 * s } else { 0.0 };
        let area = Rect::from_min_max(pos2(inner.min.x, (text_rect.max.y - area_h).max(text_rect.min.y)), pos2(inner.max.x, text_rect.max.y));
        text_rect.max.y = (area.min.y - 6.0 * s).max(text_rect.min.y);
        let head = Rect::from_min_size(area.min, vec2(area.width(), head_h));
        let hresp = ui.interact(head, egui::Id::new("ml-details-variants"), Sense::click());
        p.rect_filled(head, CornerRadius::same(4), if hresp.hovered() { t.hover } else { t.field_bg });
        let c = pos2(head.min.x + 10.0 * s, head.center().y);
        let k = 3.5 * s;
        let tri = if view.variants_open {
            vec![pos2(c.x - k, c.y - k * 0.6), pos2(c.x + k, c.y - k * 0.6), pos2(c.x, c.y + k * 0.7)]
        } else {
            vec![pos2(c.x - k * 0.6, c.y - k), pos2(c.x + k * 0.7, c.y), pos2(c.x - k * 0.6, c.y + k)]
        };
        p.add(egui::Shape::convex_polygon(tri, t.text_dim, Stroke::NONE));
        let title = format!("{} ({})", tr("Variants"), saved.len());
        p.text(pos2(head.min.x + 20.0 * s, head.center().y), Align2::LEFT_CENTER, &title, Tokens::semibold(11.5 * s), t.text);
        p.text(
            pos2(head.max.x - 8.0 * s, head.center().y),
            Align2::RIGHT_CENTER,
            tr("refined copies, each with its situation"),
            Tokens::ui(10.0 * s),
            t.text_faint,
        );
        app.auto_mut().add_response("motionLibrary.details.variants", &hresp, &title);
        if hresp.clicked() {
            view.variants_open = !view.variants_open;
        }
        for (i, v) in saved.iter().take(shown).enumerate() {
            let row = Rect::from_min_size(pos2(area.min.x, head.max.y + i as f32 * row_h), vec2(area.width(), row_h));
            if i % 2 == 1 {
                p.rect_filled(row, CornerRadius::same(3), t.field_bg.gamma_multiply(0.5));
            }
            let bw = 52.0 * s;
            let apply_v = Rect::from_min_size(pos2(row.max.x - bw - 4.0 * s, row.center().y - 10.0 * s), vec2(bw, 20.0 * s));
            let tw = (apply_v.min.x - row.min.x - 14.0 * s).max(20.0);
            let name_font = Tokens::medium(11.0 * s);
            let sum_font = Tokens::ui(10.0 * s);
            let summary = v.card.summary();
            p.text(pos2(row.min.x + 8.0 * s, row.min.y + 10.0 * s), Align2::LEFT_CENTER, elide(&p, &v.name, &name_font, tw), name_font, t.text);
            let line = if summary.is_empty() { tr("no situation recorded yet").to_string() } else { summary.clone() };
            p.text(pos2(row.min.x + 8.0 * s, row.min.y + 25.0 * s), Align2::LEFT_CENTER, elide(&p, &line, &sum_font, tw), sum_font, t.text_faint);
            app.auto_mut().add(&format!("motionLibrary.details.variant.{}", v.id), row, &format!("{}: {line}", v.name));
            if kit::button(app, ui, apply_v, &format!("motionLibrary.details.variant.{}.apply", v.id), "Apply", false) {
                menu_pick = Some(("library.applyVariant", json!({"id": v.id})));
                view.details = None;
            }
        }
        if more {
            let r = Rect::from_min_size(pos2(area.min.x, head.max.y + shown as f32 * row_h), vec2(area.width(), 16.0 * s));
            p.text(
                pos2(r.min.x + 8.0 * s, r.center().y),
                Align2::LEFT_CENTER,
                format!("+{} more (library.variants lists them all)", saved.len() - shown),
                Tokens::ui(10.0 * s),
                t.text_faint,
            );
        }
    }
    let mut body = facts.join("  ·  ");
    // Its description (editable on the Mac with the library), else the catalog's.
    let own = edit.map(|e| folder_ops().card_descriptions.get(&card.id).cloned().unwrap_or_else(|| e.description.clone()));
    match &own {
        Some(d) if !d.is_empty() => {
            body.push_str("\n\n");
            body.push_str(d);
        }
        Some(_) => body.push_str(&format!("\n\n{}", tr("No description yet: ••• ▸ Edit Description… (searchable, and agents read it)"))),
        None if !card.description.is_empty() => {
            body.push_str("\n\n");
            body.push_str(&card.description);
        }
        None => {}
    }
    if let Some(e) = &entry {
        if let Some(code) = e["payload"]["text"].as_str().filter(|c| !c.is_empty()) {
            let cut: String = code.chars().take(600).collect();
            body.push_str(&format!("\n\n{cut}{}", if code.chars().count() > 600 { "\n…" } else { "" }));
        }
        let segments: Vec<String> = e["segments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(8)
                    .map(|g| {
                        let n = g["name"].as_str().unwrap_or("Movement");
                        match g["summary"].as_str().filter(|x| !x.is_empty()) {
                            Some(sm) => format!("• {n}: {sm}"),
                            None => format!("• {n}"),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !segments.is_empty() {
            body.push_str("\n\n");
            body.push_str(&segments.join("\n"));
        }
        if let Some(cap) = e["visual_caption"].as_str().or(e["summary"].as_str()).filter(|c| !c.is_empty()) {
            body.push_str("\n\n");
            body.push_str(cap);
        }
    }
    let galley = p.layout(body, Tokens::ui(11.5 * s), t.text_dim, text_rect.width().max(40.0));
    p.with_clip_rect(text_rect.intersect(grid)).galley(text_rect.min, galley, t.text_dim);
    // Buttons.
    let bw = 84.0 * s;
    let by = inner.max.y - 24.0 * s;
    let apply_r = Rect::from_min_size(pos2(inner.max.x - bw, by), vec2(bw, 24.0 * s));
    let close_r = apply_r.translate(vec2(-(bw + 8.0 * s), 0.0));
    let desc_r = Rect::from_min_size(pos2(inner.min.x, by), vec2(110.0 * s, 24.0 * s));
    let mut apply = None;
    if kit::button(app, ui, close_r, "motionLibrary.details.close", "Close", false) || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        view.details = None;
    }
    if edit.is_some()
        && ml_backend().is_some_and(|p| p.is_file())
        && desc_r.max.x < close_r.min.x - 8.0 * s
        && kit::button(
            app,
            ui,
            desc_r,
            "motionLibrary.details.describe",
            if own.as_deref().unwrap_or("").is_empty() { "Add description" } else { "Edit description" },
            false,
        )
    {
        menu_pick = Some(("motionLibrary.card", json!({"op": "describe", "id": card.id})));
    }
    if usable(card, &local) || plug.is_some() {
        if kit::button(app, ui, apply_r, "motionLibrary.details.apply", "Apply", true) {
            apply = Some((card.id.clone(), ("motionLibrary.card", json!({"op": "use", "id": card.id}))));
            view.details = None;
        }
    } else {
        p.text(apply_r.center(), Align2::CENTER_CENTER, tr(if local.is_empty() { "On the Mac" } else { "AE only" }), Tokens::medium(11.5 * s), t.text_faint);
    }
    if let Some((cmd, params)) = menu_pick {
        app.ui_mut().motion_library = view.clone();
        if let Err(e) = app.invoke(ctx, cmd, params) {
            app.ui_mut().status = e;
        }
        *view = app.ui_mut().motion_library.clone();
    }
    apply
}

pub fn show(app: &mut dyn TimelineHost, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens();
    let ctx = ui.ctx().clone();
    let p = ui.painter().with_clip_rect(rect);
    p.rect_filled(rect, 0.0, t.panel_bg);
    let client = match app.session().motion_library() {
        Ok(c) => c,
        Err(e) => {
            let r = rect.shrink(16.0);
            p.text(pos2(r.min.x, r.min.y + 10.0), Align2::LEFT_CENTER, tr("Motion Library isn't set up on this machine"), Tokens::semibold(12.5), t.text);
            let galley = p.layout(e, Tokens::ui(11.5), t.text_dim, r.width().max(80.0));
            p.galley(pos2(r.min.x, r.min.y + 28.0), galley, t.text_dim);
            app.auto_mut().add("motionLibrary.setup", r, "Motion Library isn't set up");
            return;
        }
    };
    // First show: check the remote (the cached catalog shows meanwhile), and repaint on downloads.
    let opened = egui::Id::new("ml-opened");
    if !ctx.data(|d| d.get_temp::<bool>(opened).unwrap_or(false)) {
        ctx.data_mut(|d| d.insert_temp(opened, true));
        let c2 = ctx.clone();
        client.set_notify(Arc::new(move || c2.request_repaint()));
        kit::exec(app, "library.sync", json!({"wait": false}));
    }
    let status = client.status();
    let mut view = app.ui_mut().motion_library.clone();
    view.clamp();
    let s = view.scale as f32 / 100.0;
    let ppp = ctx.pixels_per_point();
    let mut budget = 6u32;
    let local = app.session().motion_library_local();
    let lib = local_library(&ctx, &app.session().prefs.libraries);
    let groups = group_names(&lib, &folder_ops());
    if !view.group.is_empty() && !groups.contains(&view.group) {
        view.group.clear();
    }
    let all = library_cards(&ctx, &client, &local, &lib);
    let tree = folder_tree(&ctx, &client, &local, &lib, &view.group);
    let total = if view.group.is_empty() { all.len() } else { tree.iter().filter(|r| r.depth == 0).map(|r| r.cards).sum() };
    if let Some(msg) = folder_ops().status.take() {
        app.ui_mut().status = msg;
        forget_described();
    }
    // Where files dragged in from the Finder would land: a folder row, its icon, a card.
    // Files dragged in from the Finder: the OS cursor (winit sends no pointer motion then).
    let pointer = if crate::os_drop::files_present(&ctx) {
        app.app_ref().map_or_else(|| ctx.input(|i| i.pointer.hover_pos()), |a| crate::os_drop::drop_pointer(a, &ctx))
    } else {
        ctx.input(|i| i.pointer.hover_pos())
    };
    let (mut drop_folder, mut drop_icon, mut drop_card): (Option<String>, Option<String>, Option<String>) = (None, None, None);
    let mut group_anchor: Option<Rect> = None;
    let mut folder_pick: Option<(&'static str, Value)> = None;
    let footer_h = 32.0 * s;
    let narrow = rect.width() < 460.0;
    // The collection tree stays down to small docks (a dropdown below that).
    let tree_fits = rect.width() >= 330.0;

    // The rail on the left, the quick actions on the right (hidden when the panel is narrow).
    let rail_w = (44.0 * s).round().min(rect.width() * 0.2);
    let rail_r = Rect::from_min_max(rect.min, pos2(rect.min.x + rail_w, rect.max.y));
    let tools_w = if view.show_tools && !narrow { (40.0 * s).round() } else { 0.0 };
    let tools_r = Rect::from_min_max(pos2(rect.max.x - tools_w, rect.min.y), pos2(rect.max.x, rect.max.y - footer_h));
    // Collections: a tree on the left (a dropdown when the panel is very narrow).
    let mut main = Rect::from_min_max(pos2(rail_r.max.x + 1.0, rect.min.y), pos2(rect.max.x - tools_w, rect.max.y - footer_h));
    // Curves and Tools take the whole area right of the rail (as the web panel's workspaces).
    if !view.workspace.is_empty() {
        let area = Rect::from_min_max(main.min, pos2(main.max.x, rect.max.y));
        match view.workspace.as_str() {
            "curves" => super::motion_library_curves::workspace(app, ui, &ctx, area, &mut view, s),
            "tools" => super::motion_library_tools::workspace(app, ui, &ctx, area, &mut view, s),
            // Link and History are the app's (a host goes back to the cards).
            "link" => match app.app() {
                Some(a) => super::cards::link_tab(a, ui, area, s),
                None => view.workspace.clear(),
            },
            "families" => super::motion_library_tools::families_tab(app, ui, area, s),
            "history" => match app.app() {
                Some(a) => super::history_panel::history_tab(a, ui, area, s),
                None => view.workspace.clear(),
            },
            _ => view.workspace.clear(),
        }
        rail(app, ui, &ctx, &client, &status, rail_r, &mut view, s);
        if tools_w > 0.0 {
            toolbar(app, ui, Rect::from_min_max(tools_r.min, pos2(tools_r.max.x, rect.max.y)), s);
        }
        view.clamp();
        app.ui_mut().motion_library = view;
        return;
    }
    if tree_fits && view.show_folders {
        let auto_w = (184.0 * s).min(main.width() * if narrow { 0.36 } else { 0.38 }).max(96.0);
        let side_w = if view.sidebar_width > 0.0 { (view.sidebar_width * s).min(main.width() * 0.6).max(80.0) } else { auto_w }.round();
        let side = Rect::from_min_max(main.min, pos2(main.min.x + side_w, main.max.y));
        main.min.x = side.max.x + 1.0;
        // Its right edge: drag to resize (double-click: automatic width again).
        let edge = Rect::from_min_max(pos2(side.max.x - 3.0, side.min.y), pos2(side.max.x + 3.0, side.max.y));
        let eresp = ui.interact(edge, egui::Id::new("ml-sidebar-edge"), Sense::click_and_drag()).on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        app.auto_mut().add_response("motionLibrary.sidebarResize", &eresp, "Resize the collections");
        if eresp.dragged() {
            view.sidebar_width = ((side_w + eresp.drag_delta().x) / s).clamp(80.0, 420.0);
        } else if eresp.double_clicked() {
            view.sidebar_width = 0.0;
        }
        p.line_segment(
            [side.right_top(), side.right_bottom()],
            Stroke::new(if eresp.hovered() || eresp.dragged() { 2.0 } else { 1.0 }, if eresp.dragged() { t.accent } else { t.separator }),
        );
        let head_h = 30.0 * s;
        // COLLECTIONS, or the collection group shown: click to choose a group (the web panel's
        // group picker).
        let head_label = if view.group.is_empty() { tr("COLLECTIONS").to_string() } else { view.group.to_uppercase() };
        if lib.present() {
            let label_r = Rect::from_min_max(pos2(side.min.x + 6.0 * s, side.min.y + 4.0 * s), pos2(side.max.x - 30.0 * s, side.min.y + head_h - 4.0 * s));
            let gresp = ui.interact(label_r, egui::Id::new("ml-group-pick"), Sense::click());
            if gresp.hovered() || view.popover == "groups" {
                p.rect_filled(label_r, CornerRadius::same((4.0 * s) as u8), t.hover);
            }
            let font = Tokens::semibold(10.0 * s);
            let shown = elide(&p, &format!("{head_label} ▾"), &font, label_r.width() - 8.0 * s);
            p.text(
                pos2(label_r.min.x + 6.0 * s, label_r.center().y),
                Align2::LEFT_CENTER,
                shown,
                font,
                if view.group.is_empty() { t.text_faint } else { t.accent },
            );
            app.auto_mut().add_response("motionLibrary.groups", &gresp, &head_label);
            if gresp.on_hover_text(tr("Collection groups: show one group's collections")).clicked() {
                view.popover = if view.popover == "groups" { String::new() } else { "groups".into() };
            }
            group_anchor = Some(label_r);
        } else {
            p.text(pos2(side.min.x + 12.0 * s, side.min.y + head_h / 2.0), Align2::LEFT_CENTER, &head_label, Tokens::semibold(10.0 * s), t.text_faint);
        }
        // + : a new top folder (as on the web panel).
        let plus = Rect::from_center_size(pos2(side.max.x - 16.0 * s, side.min.y + head_h / 2.0), vec2(20.0 * s, 20.0 * s));
        let presp = ui.interact(plus, egui::Id::new("ml-new-folder"), Sense::click()).on_hover_text(tr("New folder"));
        app.auto_mut().add_response("motionLibrary.newFolder", &presp, "New folder");
        if presp.hovered() {
            p.rect_filled(plus, CornerRadius::same((4.0 * s) as u8), t.hover);
        }
        let pc = if presp.hovered() { t.text } else { t.text_faint };
        let (cx, cy, arm) = (plus.center().x, plus.center().y, 4.5 * s);
        let st = Stroke::new(1.4 * s, pc);
        p.line_segment([pos2(cx - arm, cy), pos2(cx + arm, cy)], st);
        p.line_segment([pos2(cx, cy - arm), pos2(cx, cy + arm)], st);
        if presp.clicked() {
            folder_pick = Some(("motionLibrary.folder", json!({"op": "create", "category": ""})));
        }
        let list = Rect::from_min_max(pos2(side.min.x, side.min.y + head_h), side.max);
        let sp = p.with_clip_rect(list);
        let row_h = (26.0 * s).round();
        let rows = shown_rows(&tree, &view.expanded);
        let content = (rows.len() + 1) as f32 * row_h + 8.0 * s;
        if ui.rect_contains_pointer(list) {
            let dy = ctx.input(|i| i.smooth_scroll_delta.y);
            if dy.is_finite() && dy != 0.0 {
                view.folder_scroll -= dy;
            }
        }
        view.folder_scroll = view.folder_scroll.clamp(0.0, (content - list.height()).max(0.0));
        let mut toggle: Option<String> = None;
        for (i, row) in std::iter::once(None).chain(rows.iter().map(|r| Some(*r))).enumerate() {
            let y = list.min.y + i as f32 * row_h - view.folder_scroll;
            if y + row_h < list.min.y || y > list.max.y {
                continue;
            }
            let r = Rect::from_min_size(pos2(side.min.x + 6.0 * s, y), vec2(side.width() - 12.0 * s, row_h - 2.0 * s));
            let (path, name, count, depth) = match row {
                Some(row) => (row.path.as_str(), row.name.as_str(), row.cards, row.depth),
                None => ("", tr("All motion"), total, 0),
            };
            let above = i.checked_sub(2).and_then(|k| rows.get(k)).map(|r| r.path.as_str());
            let auto_id = format!("motionLibrary.folder.{}", if path.is_empty() { "all" } else { path });
            let resp = ui.interact(r.intersect(list), egui::Id::new(("ml-folder", path)), Sense::click_and_drag());
            let active = view.folder == path;
            if pointer.is_some_and(|q| r.intersect(list).contains(q)) {
                drop_folder = Some(path.to_string());
            }
            // Drag a folder to reorder it among its siblings (top or bottom edge of a row) or move
            // it into another folder (the middle); onto All motion: the top level.
            if !path.is_empty() && resp.drag_started() {
                egui::DragAndDrop::set_payload(&ctx, FolderDrag(path.to_string()));
            }
            if let Some(fd) = resp.dnd_hover_payload::<FolderDrag>()
                && fd.0 != path
                && !under(path, &fd.0)
            {
                let (to, place) = boundary_drop(&fd.0, path, drop_place(path, r, pointer), above);
                // Indented to the level the folder lands at.
                let x = r.min.x + to.matches('/').count() as f32 * 12.0 * s;
                let line = |y: f32| {
                    sp.line_segment([pos2(x, y), pos2(r.max.x, y)], Stroke::new(2.0, t.accent));
                };
                match place {
                    DropPlace::Before => line(r.min.y),
                    DropPlace::After => line(r.max.y),
                    DropPlace::Inside => {
                        sp.rect_stroke(r, CornerRadius::same((5.0 * s) as u8), Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
                    }
                }
            }
            if let Some(fd) = dropped::<FolderDrag>(&resp)
                && fd.0 != path
                && !under(path, &fd.0)
            {
                let (to, place) = boundary_drop(&fd.0, path, drop_place(path, r, pointer), above);
                folder_pick = Some(folder_drop(&fd.0, &to, place));
            }
            // Drop a card on a folder: move it there.
            if !path.is_empty()
                && let Some(payload) = resp.dnd_hover_payload::<super::DragPayload>()
                && matches!(payload.as_ref(), super::DragPayload::LibraryCard(_))
            {
                sp.rect_stroke(r, CornerRadius::same((5.0 * s) as u8), Stroke::new(1.5, t.accent), egui::StrokeKind::Inside);
            }
            if !path.is_empty()
                && let Some(payload) = dropped::<super::DragPayload>(&resp)
                && let super::DragPayload::LibraryCard(id) = payload.as_ref()
                && lib.cards.contains_key(id)
            {
                folder_pick = Some(("motionLibrary.card", json!({"op": "move", "id": id, "category": path})));
            }
            if active {
                sp.rect_filled(r, CornerRadius::same((5.0 * s) as u8), t.accent.gamma_multiply(0.28));
            } else if resp.hovered() {
                sp.rect_filled(r, CornerRadius::same((5.0 * s) as u8), t.hover);
            }
            let x0 = r.min.x + depth as f32 * 12.0 * s;
            // The open/close arrow of folders with subfolders.
            let arrow = Rect::from_min_size(pos2(x0, r.min.y), vec2(14.0 * s, r.height()));
            if let Some(row) = row
                && row.has_children
            {
                let open = view.expanded.contains(path);
                let icon = if open { crate::icons::Icon::ChevronDown } else { crate::icons::Icon::ChevronRight };
                crate::icons::paint(&sp, Rect::from_center_size(arrow.center(), vec2(10.0 * s, 10.0 * s)), icon, t.text_faint);
                let aresp = ui.interact(arrow.intersect(list), egui::Id::new(("ml-folder-toggle", path)), Sense::click());
                app.auto_mut().add(&format!("motionLibrary.folderToggle.{path}"), arrow.intersect(list), if open { "Close folder" } else { "Open folder" });
                if aresp.clicked() {
                    toggle = Some(path.to_string());
                }
            }
            let icon_r = Rect::from_center_size(pos2(arrow.max.x + 9.0 * s, r.center().y), vec2(16.0 * s, 16.0 * s));
            if !path.is_empty() && pointer.is_some_and(|q| icon_r.expand(3.0).contains(q)) {
                drop_icon = Some(path.to_string());
            }
            let color = if active || resp.hovered() { t.text } else { t.text_dim };
            let local_icon = lib.icons.get(path).map(String::as_str);
            paint_folder_icon(&ctx, &client, &sp, row, local_icon, icon_r, color, &mut budget);
            let count_s = count.to_string();
            let count_w = sp.layout_no_wrap(count_s.clone(), Tokens::ui(10.0 * s), t.text_faint).size().x;
            sp.text(pos2(r.max.x - 6.0 * s, r.center().y), Align2::RIGHT_CENTER, &count_s, Tokens::ui(10.0 * s), if active { t.text } else { t.text_faint });
            let name_r = Rect::from_min_max(pos2(icon_r.max.x + 7.0 * s, r.min.y), pos2(r.max.x - count_w - 12.0 * s, r.max.y));
            let font = Tokens::ui(12.0 * s);
            let shown = elide(&sp, name, &font, name_r.width().max(4.0));
            draw_text_clipped(&sp, name_r, &shown, font, if active { t.text } else { t.text_dim });
            app.auto_mut().add(&auto_id, r.intersect(list), name);
            if let Some(pick) = folder_menu(app.auto_mut(), &resp, path, t.danger, !groups.is_empty()) {
                folder_pick = Some(pick);
            }
            if resp.clicked() && toggle.is_none() {
                let has_children = row.is_some_and(|r| r.has_children);
                if active && has_children {
                    // Clicking the open folder again folds it.
                    toggle = Some(path.to_string());
                } else {
                    // Opening a folder ends a search (as Finder): a forgotten query hid every card.
                    view.folder = path.to_string();
                    view.query.clear();
                    view.scroll = 0.0;
                    if has_children {
                        view.expanded.insert(path.to_string());
                    }
                }
            }
        }
        if let Some(path) = toggle
            && !view.expanded.remove(&path)
        {
            view.expanded.insert(path);
        }
        // Space under the last folder: right-click for New folder… too.
        let below_y = list.min.y + (rows.len() + 1) as f32 * row_h - view.folder_scroll;
        if below_y < list.max.y {
            let below = Rect::from_min_max(pos2(list.min.x, below_y.max(list.min.y)), list.max);
            let bresp = ui.interact(below, egui::Id::new("ml-folder-empty"), Sense::click());
            if pointer.is_some_and(|q| below.contains(q)) {
                drop_folder = Some(view.folder.clone());
            }
            if let Some(pick) = folder_menu(app.auto_mut(), &bresp, "", t.danger, false) {
                folder_pick = Some(pick);
            }
        }
        // Its scroll bar (drag it, or scroll over the tree).
        let max = (content - list.height()).max(0.0);
        if max > 0.0 {
            let track = Rect::from_min_max(pos2(side.max.x - 7.0 * s, list.min.y + 2.0), pos2(side.max.x - 1.0, list.max.y - 2.0));
            let bar_h = (track.height() * list.height() / content).max(20.0 * s).min(track.height());
            let y = track.min.y + (track.height() - bar_h) * (view.folder_scroll / max);
            let bar = Rect::from_min_size(pos2(track.min.x, y), vec2(track.width(), bar_h));
            let bresp = ui.interact(track, egui::Id::new("ml-folder-scrollbar"), Sense::click_and_drag());
            app.auto_mut().add("motionLibrary.folderScroll", track, "Scroll the collections");
            if bresp.dragged() {
                view.folder_scroll += bresp.drag_delta().y * content / track.height();
            } else if bresp.clicked()
                && let Some(pos) = bresp.interact_pointer_pos()
            {
                view.folder_scroll = (pos.y - track.min.y - bar_h / 2.0) / (track.height() - bar_h).max(1.0) * max;
            }
            view.folder_scroll = view.folder_scroll.clamp(0.0, max);
            let strong = bresp.hovered() || bresp.dragged();
            sp.rect_filled(bar, CornerRadius::same(3), t.text_faint.gamma_multiply(if strong { 0.9 } else { 0.5 }));
        }
    }

    if view.popover == "groups"
        && let Some(anchor) = group_anchor
    {
        let h = (34.0 + 24.0 * (groups.len() as f32 + 1.0) + 8.0 + 24.0 * if view.group.is_empty() { 1.0 } else { 3.0 }) * s;
        popover(app, &ctx, anchor, vec2(220.0 * s, h), &mut view, |app, ui, r, view| {
            if let Some(pick) = groups_body(app, ui, r, view, &groups, s) {
                folder_pick = Some(pick);
            }
        });
    }
    if let Some((cmd, params)) = folder_pick {
        app.ui_mut().motion_library = view.clone();
        if let Err(e) = app.invoke(&ctx, cmd, params) {
            app.ui_mut().status = e;
        }
        view = app.ui_mut().motion_library.clone();
    }

    // Header: the collection and its count, the update chip or status, search, refresh, display.
    let pad = 10.0 * s;
    let head_h = 36.0 * s;
    let head = Rect::from_min_size(main.min, vec2(main.width(), head_h));
    let cards = visible_cards(&ctx, &all, &local, &lib, &tree, &view);
    let search_w = (240.0 * s).min((head.width() * 0.5).max(80.0));
    let search_r = Rect::from_min_size(pos2(head.max.x - pad - search_w, head.center().y - 12.0 * s), vec2(search_w, 24.0 * s));
    let mut q = view.query.clone();
    let resp = widgets::search_field(ui, search_r, &mut q, "Search assets", &t);
    app.auto_mut().add_response("motionLibrary.search", &resp, "Search assets");
    if ctx.data_mut(|d| d.remove_temp::<bool>(egui::Id::new("ml-focus-search"))).unwrap_or(false) {
        resp.request_focus();
    }
    if q != view.query {
        view.query = q;
        view.scroll = 0.0;
    }
    let mut title = if view.folder.is_empty() { tr("All motion").to_string() } else { view.folder.rsplit('/').next().unwrap_or(&view.folder).to_string() };
    if !view.kind_filter.is_empty() {
        title = format!("{title} · {}", view.kind_filter);
    }
    let title_font = Tokens::semibold(13.0 * s);
    let left = Rect::from_min_max(pos2(head.min.x + pad, head.min.y), pos2(search_r.min.x - 8.0 * s, head.max.y));
    let title_s = elide(&p, &title, &title_font, (left.width() * 0.6).max(10.0));
    let title_w = p.layout_no_wrap(title_s.clone(), title_font.clone(), t.text).size().x;
    draw_text_clipped(&p, left, &title_s, title_font, t.text);
    let count_s = format!("{} {}", cards.len(), if cards.len() == 1 { "item" } else { "items" });
    let count_r = Rect::from_min_max(pos2(left.min.x + title_w + 8.0 * s, left.min.y), left.max);
    draw_text_clipped(&p, count_r, &count_s, Tokens::ui(11.0 * s), t.text_faint);
    app.auto_mut().add("motionLibrary.count", count_r, &count_s);
    let count_w = p.layout_no_wrap(count_s, Tokens::ui(11.0 * s), t.text_faint).size().x;
    let info = Rect::from_min_max(pos2(count_r.min.x + count_w + 10.0 * s, head.center().y - 10.0 * s), pos2(left.max.x, head.center().y + 10.0 * s));
    if let Some(u) = &status.update {
        let w = (p.layout_no_wrap(u.label.clone(), Tokens::medium(11.0 * s), Color32::WHITE).size().x + 20.0 * s).min(info.width().max(0.0));
        let chip = Rect::from_min_size(info.min, vec2(w, info.height()));
        if w > 30.0 * s {
            let resp = ui.interact(chip, egui::Id::new("ml-update-chip"), Sense::click());
            p.rect_filled(chip, CornerRadius::same((10.0 * s) as u8), if resp.hovered() { t.accent_hover } else { t.accent });
            draw_text_clipped(&p, chip.shrink2(vec2(10.0 * s, 0.0)), &u.label, Tokens::medium(11.0 * s), Color32::WHITE);
            app.auto_mut().add("motionLibrary.updateChip", chip, &u.label);
            if resp.on_hover_text(tr("Switch to the newest library")).clicked() {
                kit::exec(app, "library.update", json!({}));
                view.scroll = 0.0;
            }
        }
    } else {
        let (text, color) = match (&status.error, status.cards) {
            (Some(e), _) => (e.clone(), t.warning),
            (None, 0) if status.refreshing || !status.checked => (tr("Loading the library…").to_string(), t.text_dim),
            (None, _) if status.downloading > 0 => (format!("downloading {}", status.downloading), t.text_faint),
            (None, _) => (format!("version {}", status.version), t.text_faint),
        };
        draw_text_clipped(&p, info, &text, Tokens::ui(10.5 * s), color);
        app.auto_mut().add("motionLibrary.status", info, &text);
    }
    let mut grid = Rect::from_min_max(pos2(main.min.x, head.max.y), main.max);
    if !tree_fits {
        let dr = Rect::from_min_size(pos2(grid.min.x + pad, grid.min.y), vec2((grid.width() - 2.0 * pad).max(60.0), 22.0 * s));
        let rows = shown_rows(&tree, &view.expanded);
        let mut paths: Vec<String> = vec![String::new()];
        let mut labels: Vec<String> = vec![format!("{}  ({total})", tr("All motion"))];
        for r in tree.iter() {
            paths.push(r.path.clone());
            labels.push(format!("{}{}  ({})", "  ".repeat(r.depth), r.name, r.cards));
        }
        let _ = rows;
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let cur = paths.iter().position(|f| *f == view.folder).unwrap_or(0);
        if let Some(i) = kit::dropdown(app, ui, dr, "motionLibrary.folder", &refs, cur)
            && let Some(f) = paths.get(i)
        {
            view.folder = f.clone();
            view.scroll = 0.0;
        }
        grid.min.y += 28.0 * s;
    }

    // Cards: folder sections (browsing everything) or packed columns.
    let gp = p.with_clip_rect(grid);
    let gap = (8.0 * s).round();
    let inner = grid.shrink2(vec2(pad, 0.0));
    let card_size = view.folder_card_sizes.get(&view.folder).copied().unwrap_or(view.card_size);
    let card_w = card_size * s;
    let compact = card_size <= COMPACT_CARD;
    let caption_h = if compact { 38.0 * s } else { 42.0 * s };
    let header_h = 30.0 * s;
    let is_grouped = grouped(&view);
    let lay = layout(&cards, is_grouped, (inner.width() - 6.0).max(40.0), card_w, gap, caption_h, header_h);
    let content_h = lay.height + gap * 2.0;
    let max_scroll = (content_h - grid.height()).max(0.0);
    if ui.rect_contains_pointer(grid) && view.popover.is_empty() && view.details.is_none() {
        let dy = ctx.input(|i| i.smooth_scroll_delta.y);
        if dy.is_finite() && dy != 0.0 {
            view.scroll -= dy;
        }
    }
    view.scroll = view.scroll.clamp(0.0, max_scroll);
    if cards.is_empty() {
        let msg = if !view.group.is_empty() && tree.is_empty() {
            "This group is empty: make a collection with +, or move one here"
        } else if total == 0 {
            ""
        } else {
            "No matching motion"
        };
        let query = view.query.trim().to_string();
        let msg = if query.is_empty() || msg.is_empty() || !view.group.is_empty() && tree.is_empty() {
            tr(msg).to_string()
        } else {
            format!("{} \u{201c}{}\u{201d}", tr("No motion matches"), elide(&gp, &query, &Tokens::ui(12.0 * s), (grid.width() - 140.0 * s).max(40.0)))
        };
        gp.text(grid.center() - vec2(0.0, 14.0 * s), Align2::CENTER_CENTER, msg, Tokens::ui(12.0 * s), t.text_faint);
        if !query.is_empty() {
            // Clear the search; searching inside a folder can also search every collection.
            let in_folder = !view.folder.is_empty();
            let w = 120.0 * s;
            let all_w = if in_folder { 170.0 * s } else { 0.0 };
            let gap = if in_folder { 8.0 * s } else { 0.0 };
            let x0 = grid.center().x - (w + gap + all_w) / 2.0;
            let y = grid.center().y + 16.0 * s;
            let clear = Rect::from_center_size(pos2(x0 + w / 2.0, y), vec2(w, 24.0 * s));
            if kit::button(app, ui, clear, "motionLibrary.clearSearch", "Clear search", true) {
                view.query.clear();
                view.scroll = 0.0;
            }
            let b = Rect::from_center_size(pos2(x0 + w + gap + all_w / 2.0, y), vec2(all_w, 24.0 * s));
            if in_folder && kit::button(app, ui, b, "motionLibrary.searchAll", "Search all collections", true) {
                view.folder.clear();
                view.scroll = 0.0;
            }
        }
    }
    let origin = pos2(inner.min.x, inner.min.y + gap - view.scroll);
    let side = thumb_side(lay.col_w, ppp);
    let radius = CornerRadius::same((6.0 * s) as u8);
    let mut hovered_card = None;
    let mut apply_now: Option<String> = None;
    // A card's right-click menu entry: a command and its params, run after the cards.
    let mut menu_pick: Option<(&'static str, Value)> = None;
    let variants = effectcraft_engine::library_variants::index(app.session());
    let can_v = can_variant(app);
    for item in &lay.items {
        match item {
            Placed::Header { folder, count, rect: hr } => {
                let r = hr.translate(origin.to_vec2());
                if r.max.y < grid.min.y || r.min.y > grid.max.y {
                    continue;
                }
                let n = format!("{count} {}", if *count == 1 { "item" } else { "items" });
                let nw = gp.layout_no_wrap(n.clone(), Tokens::ui(10.0 * s), t.text_faint).size().x;
                gp.text(pos2(r.max.x - 2.0, r.max.y - 9.0 * s), Align2::RIGHT_CENTER, &n, Tokens::ui(10.0 * s), t.text_faint);
                let font = Tokens::semibold(11.5 * s);
                let name = elide(&gp, folder, &font, (r.width() - nw - 16.0 * s).max(10.0));
                gp.text(pos2(r.min.x + 2.0, r.max.y - 9.0 * s), Align2::LEFT_CENTER, &name, font, t.text);
                gp.line_segment([pos2(r.min.x, r.max.y), pos2(r.max.x, r.max.y)], Stroke::new(1.0, t.separator));
                app.auto_mut().add(&format!("motionLibrary.section.{folder}"), r.intersect(grid), &format!("{folder} · {n}"));
            }
            Placed::Card { index, rect: cr, thumb_h } => {
                let tile = cr.translate(origin.to_vec2());
                if tile.max.y < grid.min.y || tile.min.y > grid.max.y {
                    continue;
                }
                let Some(card) = cards.get(*index) else { continue };
                let thumb_r = Rect::from_min_size(tile.min, vec2(tile.width(), *thumb_h));
                let resp = ui.interact(tile.intersect(grid), egui::Id::new(("ml-card", &card.id)), Sense::click_and_drag());
                // Drag a card onto the Timeline or the Composition viewer to apply it there.
                if pointer.is_some_and(|q| tile.intersect(grid).contains(q)) {
                    drop_card = Some(card.id.clone());
                }
                if resp.drag_started() && (usable(card, &local) || lib.cards.contains_key(&card.id)) {
                    egui::DragAndDrop::set_payload(&ctx, super::DragPayload::LibraryCard(card.id.clone()));
                }
                let selected = view.selected.as_deref() == Some(card.id.as_str());
                let hovered = resp.hovered();
                gp.rect_filled(tile, radius, if hovered { t.hover } else { t.row_alt });
                let tr_radius = CornerRadius { nw: radius.nw, ne: radius.ne, sw: 0, se: 0 };
                gp.rect_filled(thumb_r, tr_radius, t.field_bg);
                let sound = is_sound(card, &local);
                let live = if hovered && !sound { preview_frame(app, &ctx, &client, &local, card) } else { None };
                if hovered {
                    hovered_card = Some(card.id.clone());
                }
                // Hover to hear a Sound card (the app plays it; it stops when the pointer leaves).
                let sound_src = if hovered && sound { sound_source(&ctx, &client, &local, card) } else { SoundSource::None };
                if let SoundSource::File(path) = &sound_src
                    && let Some(a) = app.app()
                {
                    a.sound_hover = Some(crate::SoundHover { key: card.id.clone(), path: path.clone(), gain: view.sfx_volume });
                }
                let tex = live.map(|t| (t.id(), t.size_vec2())).or_else(|| {
                    // This Mac's library folder first (as the web panel), the bucket's copy else.
                    let thumb = match local.get(&card.id).filter(|e| !e.thumbnail.is_empty()) {
                        Some(e) => match cached_thumb(&ctx, &thumb_key(&e.thumbnail, card.crop), side) {
                            Some(Some(t)) => Some(t),
                            Some(None) => card.thumbnail.as_ref().and_then(|b| thumbnail(&ctx, &client, b, side, card.crop, &mut budget)),
                            None => local_thumbnail(&ctx, &e.thumbnail, side, card.crop, &mut budget),
                        },
                        None => card.thumbnail.as_ref().and_then(|b| thumbnail(&ctx, &client, b, side, card.crop, &mut budget)),
                    };
                    thumb.map(|(tex, [w, h])| (tex.id(), vec2(w as f32, h as f32)))
                });
                let ease = (tex.is_none() || lib.cards.get(&card.id).is_some_and(|l| !l.thumbnail_override))
                    .then(|| lib.cards.get(&card.id).and_then(|l| l.speed_graph.as_ref()).and_then(keys_curve))
                    .flatten();
                match (tex, ease) {
                    (_, Some(curve)) => paint_speed_graph(&gp, thumb_r, curve, t.accent, t.separator),
                    (Some((tid, size)), None) => paint_cover(&gp, tid, size, thumb_r),
                    (None, None) => {
                        let icon = match card.kind_label.as_str() {
                            "Sound" => crate::icons::Icon::Audio,
                            "Overlay" => crate::icons::Icon::Play,
                            _ if card.kind == "footage" => crate::icons::Icon::Footage,
                            _ => crate::icons::Icon::Comp,
                        };
                        let g = (thumb_r.height() * 0.3).clamp(10.0, 40.0 * s);
                        crate::icons::paint(&gp, Rect::from_center_size(thumb_r.center(), vec2(g, g)), icon, t.text_faint.gamma_multiply(0.7));
                    }
                }
                // A badge: footage to relink, or a card applied in After Effects on the Mac.
                let via_plug = plug_use(card, &local, &lib);
                let badge = if on_mac(card, &local) && via_plug.is_none() {
                    Some((tr(mac_words(&local).0), Color32::from_black_alpha(170), t.text))
                } else if needs_footage(card, &local) {
                    Some((tr("needs footage"), t.warning.gamma_multiply(0.9), Color32::BLACK))
                } else {
                    None
                };
                if let Some((label, fill, color)) = badge {
                    let font = Tokens::medium(9.5 * s);
                    let bw = gp.layout_no_wrap(label.to_string(), font.clone(), color).size().x + 12.0 * s;
                    let badge = Rect::from_min_size(pos2(thumb_r.min.x + 5.0 * s, thumb_r.max.y - 20.0 * s), vec2(bw, 15.0 * s));
                    if badge.max.x < thumb_r.max.x {
                        gp.rect_filled(badge, CornerRadius::same((7.0 * s) as u8), fill);
                        gp.text(badge.center(), Align2::CENTER_CENTER, label, font, color);
                    }
                }
                // Saved variants (refined copies with their situation cards): the count, which
                // opens the details' Variants row.
                let n_variants = variants.count(&card.id);
                if n_variants > 0 {
                    let font = Tokens::medium(9.5 * s);
                    let label = if thumb_r.width() > 110.0 * s {
                        format!("{n_variants} variant{}", if n_variants == 1 { "" } else { "s" })
                    } else {
                        format!("◆ {n_variants}")
                    };
                    let vw = gp.layout_no_wrap(label.clone(), font.clone(), Color32::WHITE).size().x + 12.0 * s;
                    let vr = Rect::from_min_size(pos2(thumb_r.max.x - vw - 5.0 * s, thumb_r.max.y - 20.0 * s), vec2(vw, 15.0 * s));
                    if grid.contains_rect(vr) {
                        let vresp = ui.interact(vr, egui::Id::new(("ml-card-variants", &card.id)), Sense::click());
                        gp.rect_filled(vr, CornerRadius::same((7.0 * s) as u8), if vresp.hovered() { t.accent } else { t.accent.gamma_multiply(0.75) });
                        gp.text(vr.center(), Align2::CENTER_CENTER, &label, font, Color32::WHITE);
                        app.auto_mut().add(&format!("motionLibrary.variantCount.{}", card.id), vr, &format!("{n_variants} saved variants"));
                        if vresp.on_hover_text(tr("Saved variants, each with the situation it was made for: click to see them")).clicked() {
                            view.details = Some(card.id.clone());
                            view.selected = Some(card.id.clone());
                            view.variants_open = true;
                        }
                    }
                }
                if view.applying.as_deref() == Some(card.id.as_str()) {
                    gp.rect_filled(thumb_r, tr_radius, Color32::from_black_alpha(140));
                    gp.text(thumb_r.center(), Align2::CENTER_CENTER, tr("Downloading…"), Tokens::medium(11.5 * s), Color32::WHITE);
                }
                // Not in the bucket yet: the sync mark, a one-click Save to Library.
                let saving_this = view.saving.as_ref().is_some_and(|(id, _)| *id == card.id);
                if saving_this {
                    gp.rect_filled(thumb_r, tr_radius, Color32::from_black_alpha(150));
                    let msg = view
                        .saving
                        .as_ref()
                        .and_then(|(_, job)| app.session().jobs().into_iter().find(|j| j.id == *job))
                        .map(|j| j.message)
                        .unwrap_or_default();
                    let galley = gp.layout(
                        format!("{}\n{msg}", tr("Saving to the library…")),
                        Tokens::medium(10.5 * s),
                        Color32::WHITE,
                        (thumb_r.width() - 12.0 * s).max(10.0),
                    );
                    gp.galley(thumb_r.center() - galley.size() / 2.0, galley, Color32::WHITE);
                } else if unsynced(card, &local) {
                    let sr = Rect::from_min_size(thumb_r.min + vec2(5.0 * s, 5.0 * s), vec2(20.0 * s, 16.0 * s));
                    if grid.contains_rect(sr) {
                        let sresp = ui.interact(sr, egui::Id::new(("ml-card-sync", &card.id)), Sense::click());
                        let hot = sresp.hovered();
                        gp.rect_filled(sr, CornerRadius::same((8.0 * s) as u8), if hot { t.accent } else { Color32::from_black_alpha(170) });
                        paint_cloud_up(&gp, sr.shrink2(vec2(3.0 * s, 2.0 * s)), if hot { Color32::WHITE } else { t.warning });
                        app.auto_mut().add(&format!("motionLibrary.sync.{}", card.id), sr, "Not synced: Save to Library");
                        if sresp
                            .on_hover_text(tr("Not synced: only on this Mac. Click to save it to the library (uploads its files; video stays home)"))
                            .clicked()
                        {
                            menu_pick = Some(("motionLibrary.save", json!({"id": card.id})));
                        }
                    }
                }
                if hovered && sound {
                    paint_sound_state(app, &gp, thumb_r, card, &sound_src, view.sfx_volume, s);
                }
                if selected {
                    gp.rect_stroke(tile, radius, Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
                }
                // Details: the ⓘ button over a hovered card.
                if hovered || view.details.as_deref() == Some(card.id.as_str()) {
                    let ir = Rect::from_center_size(pos2(thumb_r.max.x - 13.0 * s, thumb_r.min.y + 13.0 * s), vec2(18.0 * s, 18.0 * s));
                    if grid.contains_rect(ir) {
                        let iresp = ui.interact(ir, egui::Id::new(("ml-card-info", &card.id)), Sense::click());
                        gp.circle_filled(ir.center(), 9.0 * s, if iresp.hovered() { t.accent } else { Color32::from_black_alpha(170) });
                        gp.text(ir.center(), Align2::CENTER_CENTER, "i", Tokens::semibold(11.0 * s), Color32::WHITE);
                        app.auto_mut().add(&format!("motionLibrary.info.{}", card.id), ir, "Details and options");
                        if iresp.on_hover_text(tr("Details and options")).clicked() {
                            view.details = Some(card.id.clone());
                            view.selected = Some(card.id.clone());
                        }
                    }
                }
                // Caption: the name, then what and where it is.
                let cx = tile.min.x + 7.0 * s;
                let cw = (tile.width() - 14.0 * s).max(4.0);
                let name_font = Tokens::medium(if compact { 10.0 } else { 11.0 } * s);
                if compact {
                    let mut job = egui::text::LayoutJob::simple(card.name.clone(), name_font, t.text, cw);
                    job.wrap.max_rows = 2;
                    let galley = gp.layout_job(job);
                    gp.galley(pos2(cx, thumb_r.max.y + 6.0 * s), galley, t.text);
                } else {
                    let name = elide(&gp, &card.name, &name_font, cw);
                    gp.text(pos2(cx, thumb_r.max.y + 13.0 * s), Align2::LEFT_CENTER, &name, name_font, t.text);
                    let meta_font = Tokens::ui(10.0 * s);
                    let meta = elide(&gp, &card.subtitle(), &meta_font, cw);
                    gp.text(pos2(cx, thumb_r.max.y + 28.0 * s), Align2::LEFT_CENTER, &meta, meta_font, t.text_faint);
                }
                let label = if on_mac(card, &local) && via_plug.is_none() {
                    format!("{} ({})", card.name, mac_words(&local).0)
                } else if needs_footage(card, &local) {
                    format!("{} (needs footage)", card.name)
                } else {
                    card.name.clone()
                };
                app.auto_mut().add(&format!("motionLibrary.card.{}", card.id), tile.intersect(grid), &label);
                if resp.clicked() || resp.secondary_clicked() {
                    view.selected = Some(card.id.clone());
                }
                if let Some(pick) = card_menu(app.auto_mut(), &resp, card, &local, &lib, view.saving.is_none(), can_v, t.danger) {
                    menu_pick = Some(pick);
                }
                if resp.double_clicked() {
                    if usable(card, &local) {
                        apply_now = Some(card.id.clone());
                    } else if via_plug.is_some() {
                        menu_pick = Some(("motionLibrary.card", json!({"op": "use", "id": card.id})));
                    } else {
                        app.ui_mut().status = format!("“{}”: {}", card.name, mac_words(&local).1);
                    }
                }
                if hovered {
                    let mut tip = format!("{}\n{}", card.name, card.subtitle());
                    if needs_footage(card, &local) {
                        let names: Vec<&str> = card.placeholders.iter().map(|p| p.name.as_str()).take(3).collect();
                        tip.push_str(&format!("\nNeeds footage: {}", names.join(", ")));
                    }
                    tip.push('\n');
                    tip.push_str(match via_plug {
                        Some("ease") => "Double-click to apply this ease to the selected keyframes",
                        Some("expression") => "Double-click to apply this expression to the selected property",
                        Some(_) => "Double-click to insert it (converts its After Effects project)",
                        None if on_mac(card, &local) => mac_words(&local).1,
                        None => "Double-click to add, or drag it onto the Timeline (or onto a folder to move it)",
                    });
                    resp.on_hover_text(tip);
                }
            }
        }
    }
    if hovered_card.is_none() {
        stop_preview(&ctx);
    }
    // Scroll bar.
    if max_scroll > 0.0 {
        let frac = grid.height() / content_h;
        let bar_h = (grid.height() * frac).max(24.0);
        let y = grid.min.y + (grid.height() - bar_h) * (view.scroll / max_scroll);
        gp.rect_filled(Rect::from_min_size(pos2(grid.max.x - 5.0, y), vec2(3.0, bar_h)), CornerRadius::same(2), t.text_faint.gamma_multiply(0.6));
    }

    // Footer: the selected card and Apply.
    let footer = Rect::from_min_max(pos2(rail_r.max.x + 1.0, rect.max.y - footer_h), rect.max);
    p.line_segment([footer.left_top(), footer.right_top()], Stroke::new(1.0, t.separator));
    let sel = view.selected.as_ref().and_then(|id| find_card(&all, id));
    let ar = Rect::from_min_size(pos2(footer.max.x - 80.0 * s, footer.center().y - 11.0 * s), vec2(72.0 * s, 22.0 * s));
    if let Some(card) = &sel {
        let info = format!("{}  ·  {}{}", card.name, card.subtitle(), if needs_footage(card, &local) { "  ·  needs footage" } else { "" });
        let sel_plug = plug_use(card, &local, &lib);
        let ar = if usable(card, &local) || sel_plug.is_some() { ar } else { Rect::from_min_max(pos2(ar.min.x - 20.0 * s, ar.min.y), ar.max) };
        // Not in the bucket yet: Save to Library beside Apply.
        let save_w = 112.0 * s;
        let ar = if unsynced(card, &local) && view.saving.is_none() {
            let sr = Rect::from_min_size(pos2(ar.min.x - save_w - 8.0 * s, ar.min.y), vec2(save_w, ar.height()));
            if kit::button(app, ui, sr, "motionLibrary.saveSelected", "Save to Library", false) {
                menu_pick = Some(("motionLibrary.save", json!({"id": card.id})));
            }
            Rect::from_min_max(sr.min, ar.max)
        } else {
            ar
        };
        draw_text_clipped(
            &p,
            Rect::from_min_max(pos2(footer.min.x + 10.0 * s, footer.min.y), pos2(ar.min.x - 8.0, footer.max.y)),
            &info,
            Tokens::ui(11.5 * s),
            t.text_dim,
        );
        if usable(card, &local) {
            if kit::button(app, ui, ar, "motionLibrary.apply", "Apply", true) {
                apply_now = Some(card.id.clone());
            }
        } else if sel_plug.is_some() {
            if kit::button(app, ui, ar, "motionLibrary.apply", "Apply", true) {
                menu_pick = Some(("motionLibrary.card", json!({"op": "use", "id": card.id})));
            }
        } else {
            p.text(
                pos2(ar.max.x, ar.center().y),
                Align2::RIGHT_CENTER,
                tr(if local.is_empty() { "On the Mac" } else { "AE only" }),
                Tokens::medium(11.5 * s),
                t.text_faint,
            );
            app.auto_mut().add("motionLibrary.onMac", ar, "On the Mac");
        }
    }
    // The card's details over the grid.
    if let Some(id) = view.details.clone() {
        match find_card(&all, &id) {
            Some(card) => {
                let plug = plug_use(&card, &local, &lib);
                if let Some(a) = details_sheet(app, ui, &ctx, &client, grid, &card, &lib, plug, &mut view, s) {
                    if usable(&card, &local) {
                        apply_now = Some(a.0);
                    } else {
                        menu_pick = Some(a.1);
                    }
                }
            }
            None => view.details = None,
        }
    }
    // A card or group dialog (rename, describe, move, delete, groups).
    if let Some(mut ask) = view.card_ask.clone() {
        match card_ask_sheet(app, ui, grid, &mut ask, s) {
            Some(Some(pick)) => {
                view.card_ask = None;
                menu_pick = Some(pick);
            }
            Some(None) => view.card_ask = None,
            None => view.card_ask = Some(ask),
        }
    }
    // A folder dialog (new, rename, delete).
    if let Some(mut ask) = view.folder_ask.clone() {
        match folder_ask_sheet(app, ui, grid, &mut ask, s) {
            Some(Some(params)) => {
                view.folder_ask = None;
                menu_pick = Some(("motionLibrary.folder", params));
            }
            Some(None) => view.folder_ask = None,
            None => view.folder_ask = Some(ask),
        }
    }
    // A save over the size limit: ask first.
    if let Some(ask) = view.confirm_save.clone() {
        match confirm_save_sheet(app, ui, grid, &ask, s) {
            Some(true) => {
                view.confirm_save = None;
                menu_pick = Some(("motionLibrary.save", json!({"id": ask.id, "confirm": true})));
            }
            Some(false) => view.confirm_save = None,
            None => {}
        }
    }
    // A save running: when it's done, a size to confirm (the job's result), else nothing to do
    // (it said how it went).
    if let Some((id, job)) = view.saving.clone() {
        match app.session().job_log.iter().rev().find(|r| r.id == job) {
            Some(rec) => {
                view.saving = None;
                if rec.status == "done" && rec.result["needsConfirm"] == true {
                    view.confirm_save = Some(SaveAsk {
                        id,
                        name: rec.result["name"].as_str().unwrap_or_default().to_string(),
                        mb: rec.result["uploadMB"].as_f64().unwrap_or(0.0),
                        note: skipped_note(&rec.result),
                    });
                }
            }
            None if app.session().jobs().iter().any(|j| j.id == job) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
            None => view.saving = None,
        }
    }
    // A card being dragged: its name under the pointer.
    if let Some(payload) = egui::DragAndDrop::payload::<super::DragPayload>(&ctx)
        && let super::DragPayload::LibraryCard(id) = payload.as_ref()
        && let Some(ptr) = ctx.pointer_hover_pos()
    {
        let name = find_card(&all, id).map(|c| c.name).unwrap_or_default();
        let lp = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("ml-drag")));
        let g = lp.layout_no_wrap(format!("＋ {name}"), Tokens::medium(11.5), Color32::WHITE);
        let r = Rect::from_min_size(ptr + vec2(14.0, 10.0), g.size()).expand2(vec2(8.0, 4.0));
        lp.rect_filled(r, CornerRadius::same(6), t.accent);
        lp.galley(r.min + vec2(8.0, 4.0), g, Color32::WHITE);
    }
    // Files dragged in from the Finder: saved as cards of the folder under the pointer (else the
    // open one); an image on a folder's icon becomes its icon; Option-drop on a card: its
    // thumbnail. Taken here, so the app doesn't import them into the project too.
    if pointer.is_some_and(|q| rect.contains(q)) && lib.present() {
        let hovered = ctx.input(|i| i.raw.hovered_files.len());
        let alt = ctx.input(|i| i.modifiers.alt);
        let is_image = |p: &str| {
            let e = p.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
            matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")
        };
        let folder_name = |f: &Option<String>| {
            f.clone().filter(|f| !f.is_empty()).unwrap_or_else(|| if view.folder.is_empty() { "My motion".into() } else { view.folder.clone() })
        };
        if hovered > 0 {
            let hint = match (&drop_card, &drop_icon) {
                (Some(_), _) if alt => tr("Drop to make it this card's thumbnail").to_string(),
                (_, Some(f)) => format!("Drop an image to set the icon of {f}"),
                _ => format!("Drop to save in {}", folder_name(&drop_folder)),
            };
            let lp = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("ml-file-drop")));
            lp.rect_stroke(rect.shrink(4.0), CornerRadius::same(6), Stroke::new(2.0, t.accent), egui::StrokeKind::Inside);
            let g = lp.layout_no_wrap(hint.clone(), Tokens::medium(12.0 * s), Color32::WHITE);
            let hr = Rect::from_center_size(pos2(rect.center().x, rect.max.y - 40.0 * s), g.size() + vec2(20.0 * s, 10.0 * s));
            lp.rect_filled(hr, CornerRadius::same(6), t.accent);
            lp.galley(hr.min + vec2(10.0 * s, 5.0 * s), g, Color32::WHITE);
            app.auto_mut().add("motionLibrary.dropTarget", hr, &hint);
            ctx.input_mut(|i| i.raw.hovered_files.clear());
        }
        let dropped: Vec<String> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path()).filter(|p| p.is_absolute()).map(|p| p.to_string_lossy().to_string()).collect());
        if !dropped.is_empty() {
            ctx.input_mut(|i| i.raw.dropped_files.clear());
            let first = dropped.first().cloned().unwrap_or_default();
            menu_pick = Some(match (&drop_card, &drop_icon) {
                (Some(id), _) if alt => ("motionLibrary.card", json!({"op": "setThumbnail", "id": id, "path": first})),
                (_, Some(f)) if dropped.len() == 1 && is_image(&first) => ("motionLibrary.folder", json!({"op": "setIcon", "category": f, "path": first})),
                _ => ("motionLibrary.importFiles", json!({"paths": dropped, "category": folder_name(&drop_folder)})),
            });
        }
    }
    if let Some((cmd, params)) = menu_pick {
        // Through the command layer (as menus and agents run them), on the view in hand.
        app.ui_mut().motion_library = view.clone();
        if let Err(e) = app.invoke(&ctx, cmd, params) {
            app.ui_mut().status = e;
        }
        view = app.ui_mut().motion_library.clone();
    }
    if let Some(id) = apply_now {
        view.selected = Some(id.clone());
        view.applying = Some(id);
        view.apply_time = None;
    }
    // Applying: poll until the downloads are in, then the import runs (one undo step).
    if let Some(id) = view.applying.clone() {
        match app.session_mut().execute("library.apply", json!({"id": id, "wait": false, "time": view.apply_time})) {
            Ok(r) if r.get("pending").and_then(Value::as_bool) == Some(true) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                view.applying = None;
                view.apply_time = None;
            }
            Err(e) => {
                app.ui_mut().status = e.to_string();
                view.applying = None;
                view.apply_time = None;
            }
        }
    }
    rail(app, ui, &ctx, &client, &status, rail_r, &mut view, s);
    if tools_w > 0.0 {
        toolbar(app, ui, tools_r, s);
    }
    view.clamp();
    app.ui_mut().motion_library = view;
}

/// `motionLibrary.view`: change what the panel shows (`folder`, `query`, `scale`, `cardSize`,
/// `expand` / `collapse` folders, `kind`, `hereOnly`, `showFolders`, `showTools`, `popover` /
/// `display`, `details`: a card id or null); returns the view.
pub fn set_view(view: &mut ViewState, p: &Value) -> Result<Value, String> {
    if let Some(f) = p.get("folder") {
        let f = f.as_str().ok_or("`folder` is a folder path (\"\" = All motion)")?.trim().trim_matches('/');
        view.folder = f.to_string();
        view.reveal(f);
        view.scroll = 0.0;
    }
    if let Some(q) = p.get("query") {
        view.query = q.as_str().ok_or("`query` is text")?.to_string();
        view.scroll = 0.0;
    }
    if let Some(sc) = p.get("scale") {
        let sc = sc.as_f64().filter(|v| v.is_finite()).ok_or("`scale` is a percentage (80–200)")?;
        view.scale = ((sc / 10.0).round() * 10.0).clamp(SCALE_RANGE.0 as f64, SCALE_RANGE.1 as f64) as u32;
    }
    if let Some(c) = p.get("cardSize") {
        let c = c.as_f64().filter(|v| v.is_finite()).ok_or("`cardSize` is a width in points (88–1200)")?;
        view.card_size = (c as f32).clamp(CARD_SIZE_RANGE.0, CARD_SIZE_RANGE.1).round();
    }
    let paths = |v: &Value| -> Vec<String> {
        match v {
            Value::String(s) => vec![s.trim_matches('/').to_string()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).map(|s| s.trim_matches('/').to_string()).collect(),
            _ => vec![],
        }
    };
    if let Some(e) = p.get("expand") {
        for f in paths(e) {
            view.reveal(&f);
            view.expanded.insert(f);
        }
    }
    if let Some(c) = p.get("collapse") {
        for f in paths(c) {
            view.expanded.remove(&f);
        }
    }
    if let Some(d) = p.get("display").and_then(Value::as_bool) {
        view.popover = if d { "display".into() } else { String::new() };
    }
    if let Some(v) = p.get("popover") {
        let v = v.as_str().unwrap_or("");
        if !["", "display", "filters", "save"].contains(&v) {
            return Err(format!("unknown popover `{v}` (display, filters, save, or \"\")"));
        }
        view.popover = v.to_string();
    }
    if let Some(w) = p.get("workspace") {
        let w = w.as_str().unwrap_or("");
        if !["", "assets", "curves", "tools", "link", "families", "history"].contains(&w) {
            return Err(format!("unknown workspace `{w}` (assets, curves, tools, link, families, history)"));
        }
        view.workspace = if w == "assets" { String::new() } else { w.to_string() };
    }
    if let Some(v) = p.get("sfxVolume") {
        view.sfx_volume = v.as_f64().ok_or("`sfxVolume` is the SFX preview volume, 0–1")? as f32;
    }
    if let Some(k) = p.get("kind") {
        view.kind_filter = k.as_str().ok_or("`kind` is a card kind (Comp, Overlay, Sound…; \"\" = all)")?.to_string();
        view.scroll = 0.0;
    }
    for (key, slot) in [("hereOnly", &mut view.here_only), ("showFolders", &mut view.show_folders), ("showTools", &mut view.show_tools)] {
        if let Some(b) = p.get(key) {
            *slot = b.as_bool().ok_or(format!("`{key}` is true or false"))?;
        }
    }
    if let Some(d) = p.get("details") {
        view.details = d.as_str().map(str::to_string);
    }
    if let Some(v) = p.get("variants") {
        view.variants_open = v.as_bool().ok_or("`variants` is true or false (the details' Variants row open)")?;
    }
    if let Some(sel) = p.get("select") {
        view.selected = sel.as_str().map(str::to_string);
    }
    view.clamp();
    Ok(view.to_json())
}

/// `motionLibrary.save`: save a card of this Mac's library folder to the bucket in the background
/// (`library.saveCard`); over 50 MB the panel asks first (`confirm: true` uploads anyway).
pub fn start_save(app: &mut dyn TimelineHost, p: &Value) -> Result<Value, String> {
    let id = p.get("id").and_then(Value::as_str).ok_or("missing `id` (a Motion Library card id)")?.to_string();
    if let Some((busy, _)) = &app.ui_mut().motion_library.saving {
        return Err(format!("still saving `{busy}`"));
    }
    let mut params = json!({"id": id, "confirm": p.get("confirm").and_then(Value::as_bool).unwrap_or(false), "wait": false});
    if let Some(mb) = p.get("askAboveMB") {
        params["askAboveMB"] = mb.clone();
    }
    let r = app.session_mut().execute("library.saveCard", params).map_err(|e| e.to_string())?;
    let job = r["job"].as_str().unwrap_or_default().to_string();
    app.ui_mut().motion_library.confirm_save = None;
    app.ui_mut().motion_library.saving = Some((id.clone(), job.clone()));
    Ok(json!({"saving": id, "job": job}))
}

/// `motionLibrary.apply` (a drop on the Timeline or viewer, agents; `time`: comp seconds, default
/// the playhead): an After Effects card this Mac converts (comp, layers, ease, expression) goes
/// through the Motion Library plug like its Apply button; the rest downloads and imports.
pub fn apply_command(app: &mut dyn TimelineHost, ctx: &egui::Context, p: &Value) -> Result<Value, String> {
    let id = p.get("id").and_then(Value::as_str).ok_or("missing `id` (a Motion Library card)")?;
    let lib = local_library(ctx, &app.session().prefs.libraries);
    let local = app.session().motion_library_local();
    let card = app.session().motion_library().ok().and_then(|client| library_cards(ctx, &client, &local, &lib).iter().find(|c| c.id == id).cloned());
    if card.is_some_and(|c| plug_use(&c, &local, &lib).is_some()) {
        let mut use_p = json!({"op": "use", "id": id});
        if let Some(t) = p.get("time") {
            use_p["time"] = t.clone();
        }
        app.ui_mut().motion_library.selected = Some(id.to_string());
        return card_command(app, ctx, &use_p);
    }
    start_apply(&mut app.ui_mut().motion_library, p)
}

/// EMBED13: the panel's own commands for a host that shows it (FigCraft's card Timeline): the
/// `motionLibrary.*` ids and `ml.saveTextPreset`, which EffectCraft's menus run with its app
/// (`menus/motion_library.rs`), run on the host. `None` for any other id.
pub fn hosted_command(app: &mut dyn TimelineHost, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let r = match id {
        "motionLibrary.view" => set_view(&mut app.ui_mut().motion_library, params),
        "motionLibrary.apply" => apply_command(app, ctx, params),
        "motionLibrary.copyName" => (|| {
            let id = params.get("id").and_then(Value::as_str).ok_or("missing `id` (a Motion Library card id)")?;
            let name = card_name(app, ctx, id).ok_or(format!("no Motion Library card `{id}`"))?;
            ctx.copy_text(name.clone());
            Ok(json!({"copied": name}))
        })(),
        "motionLibrary.save" => start_save(app, params),
        "motionLibrary.folder" => folder_command(app, ctx, params),
        "motionLibrary.saveSelection" => save_selection_command(app, ctx, params),
        "motionLibrary.card" => card_command(app, ctx, params),
        "motionLibrary.group" => group_command(app, ctx, params),
        "motionLibrary.importFiles" => import_files_command(app, ctx, params),
        "motionLibrary.refresh" => refresh_command(app, ctx, params),
        "motionLibrary.curve" => super::motion_library_curves::command(app, ctx, params),
        "motionLibrary.tool" => super::motion_library_tools::command(app, ctx, params),
        "ml.saveTextPreset" => save_text_preset(app, ctx, params),
        _ => return None,
    };
    Some(r)
}

/// EffectCraft's settings folder on this computer (`None` in the browser), for
/// [`hosted_setup`].
pub fn user_settings_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_arch = "wasm32")]
    return None;
    #[cfg(not(target_arch = "wasm32"))]
    effectcraft_host::config_dir()
}

/// EMBED13: what the panel needs in a host's session that has no settings store (FigCraft's card
/// Timeline opens the project without one): `motion-library.json` and the rest read from
/// EffectCraft's settings folder `dir` through a store that refuses every write (a host never
/// changes EffectCraft's settings; saving to the library goes through the library's own code, as
/// in EffectCraft), and the library folders of the person's `prefs.json`. Nothing when the
/// session has a settings store already or there is no folder.
pub fn hosted_setup(session: &mut effectcraft_engine::Session, dir: Option<std::path::PathBuf>) {
    if session.config.is_some() {
        return;
    }
    let Some(dir) = dir.filter(|d| d.is_dir()) else { return };
    let store = ReadOnlySettings { dir };
    if session.prefs.libraries.is_empty()
        && let Some(text) = effectcraft_engine::config::ConfigStore::read(&store, effectcraft_engine::prefs::PREFS_FILE)
    {
        session.prefs.libraries = effectcraft_engine::prefs::Prefs::from_json(&text).libraries;
    }
    session.config = Some(Arc::new(store));
}

/// EffectCraft's settings folder, read only ([`hosted_setup`]).
struct ReadOnlySettings {
    dir: std::path::PathBuf,
}

/// The largest settings entry [`ReadOnlySettings`] reads.
const MAX_SETTING_BYTES: u64 = 16 << 20;

impl effectcraft_engine::config::ConfigStore for ReadOnlySettings {
    fn read(&self, name: &str) -> Option<String> {
        let rel = std::path::Path::new(name);
        if rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return None;
        }
        let path = self.dir.join(rel);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_SETTING_BYTES {
            return None;
        }
        std::fs::read_to_string(path).ok()
    }
    fn write(&self, _name: &str, _data: &str) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EffectCraft's settings are read only here"))
    }
    fn remove(&self, _name: &str) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EffectCraft's settings are read only here"))
    }
}

/// Apply a card without blocking; the panel shows the download.
pub fn start_apply(view: &mut ViewState, p: &Value) -> Result<Value, String> {
    let id = p.get("id").and_then(Value::as_str).ok_or("missing `id` (a Motion Library card)")?;
    view.selected = Some(id.to_string());
    view.applying = Some(id.to_string());
    view.apply_time = p.get("time").and_then(Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0);
    Ok(json!({"applying": id, "time": view.apply_time}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, cat: &str, size: Option<[u32; 2]>) -> Card {
        Card { id: id.into(), name: id.into(), category: cat.into(), thumb_size: size, ..Default::default() }
    }

    #[test]
    fn sections_rows_and_masonry() {
        let cards = vec![card("a", "A", Some([160, 100])), card("b", "A", Some([100, 200])), card("c", "A", None), card("d", "B", Some([100, 100]))];
        // 2 columns of 100: A's first row is as tall as its tallest card (b: 200 + caption 40).
        let l = layout(&cards, true, 210.0, 100.0, 10.0, 40.0, 30.0);
        assert_eq!(l.col_w, 100.0);
        let rects: Vec<(bool, f32, f32)> = l
            .items
            .iter()
            .map(|i| match i {
                Placed::Header { rect, .. } => (true, rect.min.x, rect.min.y),
                Placed::Card { rect, .. } => (false, rect.min.x, rect.min.y),
            })
            .collect();
        assert_eq!(rects, vec![(true, 0.0, 0.0), (false, 0.0, 40.0), (false, 110.0, 40.0), (false, 0.0, 290.0), (true, 0.0, 403.0), (false, 0.0, 443.0)]);
        assert!(matches!(&l.items[0], Placed::Header { count: 3, .. }));
        // Packed: d goes under a (the shorter column).
        let l = layout(&cards, false, 210.0, 100.0, 10.0, 40.0, 30.0);
        let ys: Vec<(f32, f32)> =
            l.items.iter().filter_map(|i| if let Placed::Card { rect, .. } = i { Some((rect.min.x, rect.min.y)) } else { None }).collect();
        assert_eq!(ys, vec![(0.0, 0.0), (110.0, 0.0), (0.0, 113.0), (0.0, 226.0)]);
        // A card wider than the grid takes the whole width.
        assert_eq!(layout(&cards, false, 80.0, 1200.0, 10.0, 40.0, 30.0).col_w, 80.0);
    }

    #[test]
    fn png_frames_play_in_natural_order() {
        let dir = std::env::temp_dir().join(format!("ml-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["f10.png", "f2.png", "f1.png", "notes.txt", "F3.PNG"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let names: Vec<String> =
            png_frames(&dir.to_string_lossy()).iter().map(|p| std::path::Path::new(p).file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["f1.png", "f2.png", "F3.PNG", "f10.png"]);
        assert!(png_frames("").is_empty() && png_frames("/no/such/dir").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_command_clamps_and_reveals() {
        let mut v = ViewState::default();
        let r = set_view(&mut v, &json!({"scale": 275, "cardSize": 20, "folder": "Devin Jatho/Instagram/Reels"})).unwrap();
        assert_eq!((v.scale, v.card_size), (200, 88.0));
        assert!(v.expanded.contains("Devin Jatho") && v.expanded.contains("Devin Jatho/Instagram"));
        assert_eq!(r["folder"], "Devin Jatho/Instagram/Reels");
        set_view(&mut v, &json!({"scale": 84, "collapse": "Devin Jatho", "cardSize": 400.4})).unwrap();
        assert_eq!((v.scale, v.card_size), (80, 400.0));
        assert!(!v.expanded.contains("Devin Jatho"));
        assert!(set_view(&mut v, &json!({"scale": "big"})).is_err());
        let mut odd = ViewState { scale: 0, card_size: f32::NAN, ..Default::default() };
        odd.clamp();
        assert_eq!((odd.scale, odd.card_size), (80, DEFAULT_CARD_SIZE));
        // An old workspace without the new settings.
        let old: ViewState = serde_json::from_value(json!({"query": "x"})).unwrap();
        assert_eq!((old.scale, old.card_size), (100, DEFAULT_CARD_SIZE));
    }
}

// ── Collection folders (right-click a folder, + beside COLLECTIONS) ─────────────────────────────
//
// As in the browser panel (:8765): New subfolder…, Rename…, Choose icon…, Restore default icon,
// Delete folder…. The local library's backend (`backend.py rpc`) does the work, then the library
// index is rebuilt in the background; the tree shows the change at once.

/// A folder dialog over the panel: a name to type (`create`, `rename`) or a delete to confirm.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderAsk {
    pub op: String,
    /// The folder (`create`: the parent, empty = a top folder).
    pub category: String,
    pub text: String,
    /// `delete`: what goes with the folder (the backend's `folder_contents`).
    pub contents: Value,
}

/// Changes made in this run, shown before the index has them (and over the published catalog).
struct FolderOps {
    created: Vec<String>,
    deleted: Vec<String>,
    renamed: Vec<(String, String)>,
    /// Card edits: name, folder, description, chosen thumbnail (`None`: the default again).
    card_names: BTreeMap<String, String>,
    card_folders: BTreeMap<String, String>,
    card_descriptions: BTreeMap<String, String>,
    card_thumbs: BTreeMap<String, Option<String>>,
    cards_deleted: BTreeSet<String>,
    /// Folder positions among their siblings (a reorder).
    order: BTreeMap<String, i64>,
    /// Top folders' collection group (`""`: none), and groups added, renamed, deleted.
    folder_groups: BTreeMap<String, String>,
    groups_added: Vec<String>,
    groups_renamed: Vec<(String, String)>,
    groups_deleted: Vec<String>,
    revision: u64,
    status: Option<String>,
}

static FOLDER_OPS: Mutex<FolderOps> = Mutex::new(FolderOps {
    created: Vec::new(),
    deleted: Vec::new(),
    renamed: Vec::new(),
    card_names: BTreeMap::new(),
    card_folders: BTreeMap::new(),
    card_descriptions: BTreeMap::new(),
    card_thumbs: BTreeMap::new(),
    cards_deleted: BTreeSet::new(),
    order: BTreeMap::new(),
    folder_groups: BTreeMap::new(),
    groups_added: Vec::new(),
    groups_renamed: Vec::new(),
    groups_deleted: Vec::new(),
    revision: 0,
    status: None,
});

fn folder_ops() -> std::sync::MutexGuard<'static, FolderOps> {
    FOLDER_OPS.lock().unwrap_or_else(|e| e.into_inner())
}

fn under(path: &str, folder: &str) -> bool {
    path == folder || path.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/'))
}

fn renamed_path(path: &str, renamed: &[(String, String)]) -> String {
    let mut p = path.to_string();
    for (old, new) in renamed {
        if under(&p, old) {
            p = format!("{new}{}", &p[old.len()..]);
        }
    }
    p
}

/// How the tree orders siblings: the library's saved positions (`folder_order`, then this run's
/// reorders), else top folders in `priorities` order, else by name (the web panel's `allFolders`).
#[derive(Default)]
struct Sorting {
    order: BTreeMap<String, i64>,
    priorities: Vec<String>,
}

impl Sorting {
    fn cmp(&self, a: &str, b: &str, top: bool) -> std::cmp::Ordering {
        let (ai, bi) = (self.order.get(a), self.order.get(b));
        if ai.is_some() || bi.is_some() {
            return match (ai, bi) {
                (Some(x), Some(y)) => x.cmp(y).then_with(|| a.cmp(b)),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
        }
        let rank = |f: &str| if top { self.priorities.iter().position(|p| p == f).unwrap_or(99) } else { 99 };
        rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
    }
}

/// The published tree merged with the local library's folders and this run's changes: renames
/// applied, deleted folders gone, new ones under their parents (after the existing ones), the
/// folders of cards moved elsewhere. `counts`: cards per folder (its subfolders' too) instead of
/// the catalog's; `sorting`: the library's order instead of the catalog's.
fn merge_folders(
    catalog: &[FolderRow],
    local: &[String],
    ops: &FolderOps,
    card_folders: &[String],
    counts: Option<&BTreeMap<String, usize>>,
    sorting: Option<&Sorting>,
) -> Vec<FolderRow> {
    let mut order: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cards = BTreeMap::new();
    let mut icons = BTreeMap::new();
    let mut add = |p: String, order: &mut Vec<String>| {
        if p.is_empty() || ops.deleted.iter().any(|d| under(&p, d)) {
            return;
        }
        // Parents first, so every folder has its row above it.
        let mut parts = Vec::new();
        let mut cur = p.as_str();
        while let Some((parent, _)) = cur.rsplit_once('/') {
            parts.push(parent.to_string());
            cur = parent;
        }
        for q in parts.into_iter().rev().chain(std::iter::once(p)) {
            if seen.insert(q.clone()) {
                order.push(q);
            }
        }
    };
    for row in catalog {
        let p = renamed_path(&row.path, &ops.renamed);
        cards.insert(p.clone(), row.cards);
        if let Some(icon) = &row.icon {
            icons.insert(p.clone(), icon.clone());
        }
        add(p, &mut order);
    }
    for p in local.iter().chain(ops.created.iter()).chain(card_folders.iter()) {
        add(renamed_path(p.trim_matches('/'), &ops.renamed), &mut order);
    }
    // Depth-first, siblings in first-seen order (or the library's).
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for p in &order {
        children.entry(p.rsplit_once('/').map_or("", |(parent, _)| parent)).or_default().push(p);
    }
    if let Some(sorting) = sorting {
        for (parent, kids) in children.iter_mut() {
            let top = parent.is_empty();
            kids.sort_by(|a, b| sorting.cmp(a, b, top));
        }
    }
    let mut rows = Vec::with_capacity(order.len());
    let mut stack: Vec<&str> = children.get("").map(|c| c.iter().rev().copied().collect()).unwrap_or_default();
    while let Some(p) = stack.pop() {
        let kids = children.get(p);
        rows.push(FolderRow {
            path: p.to_string(),
            name: p.rsplit('/').next().unwrap_or(p).to_string(),
            depth: p.matches('/').count(),
            cards: counts.map_or_else(|| cards.get(p).copied().unwrap_or(0), |c| c.get(p).copied().unwrap_or(0)),
            has_children: kids.is_some_and(|k| !k.is_empty()),
            icon: icons.get(p).cloned(),
        });
        if let Some(kids) = kids {
            stack.extend(kids.iter().rev().copied());
        }
    }
    rows
}

/// A card as this Mac's library index lists it (`entries` and `catalog_only` of `library.json`):
/// the names, folders and descriptions edited here win over the published catalog's.
#[derive(Clone, Debug, Default)]
pub(crate) struct LocalCard {
    pub name: String,
    pub category: String,
    /// The card's own description (the catalog's is that plus generated search words).
    pub description: String,
    pub kind_label: String,
    /// `effectcraft`, `ae` or `any`: who saved it.
    pub app: String,
    pub thumbnail_override: bool,
    /// An Ease card's two keyframes (its speed graph).
    pub speed_graph: Option<Value>,
}

/// The local library's index (`library.json`): its collection settings (folders, icons, order,
/// groups) and its cards, read again when the file changes.
#[derive(Default)]
pub(crate) struct LocalLib {
    pub folders: Vec<String>,
    pub icons: BTreeMap<String, String>,
    pub order: BTreeMap<String, i64>,
    /// Top folder → collection group.
    pub groups: BTreeMap<String, String>,
    pub group_names: Vec<String>,
    pub priorities: Vec<String>,
    pub cards: BTreeMap<String, LocalCard>,
    /// The index lists every card of the library (it has `catalog_only`): a published card it
    /// doesn't list was deleted here.
    pub complete: bool,
}

impl LocalLib {
    /// This Mac has the library (its index was read).
    pub fn present(&self) -> bool {
        !self.cards.is_empty() || !self.folders.is_empty()
    }
}

fn parse_local_lib(values: &[Value]) -> LocalLib {
    let mut lib = LocalLib::default();
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    for v in values {
        let c = &v["collections"];
        if let Some(list) = c["folders"].as_array() {
            lib.folders.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(map) = c["icons"].as_object() {
            lib.icons.extend(map.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))));
        }
        if let Some(map) = c["order"].as_object() {
            lib.order.extend(map.iter().filter_map(|(k, v)| v.as_i64().map(|n| (k.clone(), n))));
        }
        if let Some(map) = c["groups"].as_object() {
            lib.groups.extend(map.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))));
        }
        if let Some(list) = c["group_names"].as_array() {
            lib.group_names.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(list) = c["priorities"].as_array()
            && lib.priorities.is_empty()
        {
            lib.priorities = list.iter().filter_map(Value::as_str).map(str::to_string).collect();
        }
        lib.complete |= v.get("catalog_only").is_some_and(Value::is_array);
        for key in ["entries", "catalog_only"] {
            for e in v[key].as_array().map(Vec::as_slice).unwrap_or_default() {
                let id = text(&e["id"]);
                if id.is_empty() || lib.cards.contains_key(&id) {
                    continue;
                }
                lib.cards.insert(
                    id,
                    LocalCard {
                        name: text(&e["name"]),
                        category: text(&e["category"]),
                        description: text(&e["raw_description"]),
                        kind_label: text(&e["type_label"]),
                        app: text(&e["app"]),
                        thumbnail_override: e["thumbnail_override"] == true,
                        speed_graph: e.get("speed_graph").filter(|g| g.is_array()).cloned(),
                    },
                );
            }
        }
    }
    lib
}

/// The local library's index, by file stamp (`library.json` of each library folder).
pub(crate) fn local_library(ctx: &egui::Context, libraries: &[String]) -> Arc<LocalLib> {
    let paths: Vec<std::path::PathBuf> = libraries.iter().map(|f| std::path::Path::new(f).join("library.json")).collect();
    let stamp: Vec<Option<std::time::SystemTime>> = paths.iter().map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()).collect();
    let id = egui::Id::new("ml-local-library");
    if let Some((s, f)) = ctx.data(|d| d.get_temp::<(Vec<Option<std::time::SystemTime>>, Arc<LocalLib>)>(id))
        && s == stamp
    {
        return f;
    }
    let values: Vec<Value> = paths.iter().filter_map(|p| std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())).collect();
    let lib = Arc::new(parse_local_lib(&values));
    ctx.data_mut(|d| d.insert_temp(id, (stamp, lib.clone())));
    lib
}

/// The Motion Library backend script on this Mac (`EFFECTCRAFT_ML_BACKEND`, else the usual place).
fn ml_backend() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("EFFECTCRAFT_ML_BACKEND") {
        return Some(p.into());
    }
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Developer/AE Motion graphics/integrations/motion-library/backend.py"))
}

/// One backend operation (`backend.py rpc`): its result, or its error message.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn backend_rpc(op: &str, args: Value) -> Result<Value, String> {
    ml_rpc("backend.py", &["rpc"], None, op, args)
}

/// One operation of a Motion Library script (`backend.py rpc`, `effectcraft_plug.py`): the same
/// envelope, stdin `{operation, args}` → stdout `{ok, result | error}`. `port`: this app's control
/// port, for scripts that work on the open project.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn ml_rpc(name: &str, script_args: &[&str], port: Option<u16>, op: &str, args: Value) -> Result<Value, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let backend = ml_backend().filter(|p| p.is_file()).ok_or("the Motion Library backend (backend.py) isn't on this computer (set EFFECTCRAFT_ML_BACKEND)")?;
    let dir = backend.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let script = dir.join(name);
    let mut cmd = Command::new("/usr/bin/python3");
    cmd.arg(&script).args(script_args);
    if let Some(port) = port {
        cmd.env("ELIAS_EFFECTCRAFT_PORT", port.to_string());
    }
    let mut child = cmd
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't start the Motion Library backend: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json!({"operation": op, "args": args}).to_string().as_bytes()).map_err(|e| format!("Motion Library backend: {e}"))?;
    }
    let out = child.wait_with_output().map_err(|e| format!("Motion Library backend: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = serde_json::from_str::<Value>(text.trim()).ok().or_else(|| text.lines().rev().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok()));
    let Some(v) = v else {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("Motion Library backend: {}", err.trim().lines().last().unwrap_or("no answer")));
    };
    if v["ok"] == true { Ok(v["result"].clone()) } else { Err(v["error"].as_str().unwrap_or("the Motion Library backend failed").to_string()) }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn backend_rpc(_op: &str, _args: Value) -> Result<Value, String> {
    Err("Motion Library folders can be changed in the desktop app".into())
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn ml_rpc(_name: &str, _script_args: &[&str], _port: Option<u16>, _op: &str, _args: Value) -> Result<Value, String> {
    Err("saving to the Motion Library works in the desktop app".into())
}

/// Index rebuilds asked for, and the last one a finished rebuild covered.
#[cfg(not(target_arch = "wasm32"))]
static INDEX_ASKED: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(target_arch = "wasm32"))]
static INDEX_DONE: Mutex<usize> = Mutex::new(0);

/// Rebuild the local library's index (`effectcraft_library.py index`) so the panel reads the change.
/// One at a time, and skipped when a rebuild that started after this change already ran (parallel
/// rebuilds raced: an older one finishing last wrote a stale index).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn reindex_library() {
    let mine = INDEX_ASKED.fetch_add(1, Ordering::SeqCst).saturating_add(1);
    let mut done = INDEX_DONE.lock().unwrap_or_else(|e| e.into_inner());
    if *done >= mine {
        return;
    }
    let covers = INDEX_ASKED.load(Ordering::SeqCst);
    let Some(dir) = ml_backend().and_then(|p| p.parent().map(|d| d.to_path_buf())) else { return };
    let _ = std::process::Command::new("/usr/bin/nice")
        .args(["-n", "10", "/usr/bin/python3", "effectcraft_library.py", "index"])
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    *done = covers;
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn reindex_library() {}

/// What a finished operation changes in the panel right away (before the index has it).
pub(crate) enum FolderChange {
    None,
    Created(String),
    Renamed(String, String),
    Deleted(String),
    CardName(String, String),
    CardFolder(String, String),
    CardDescription(String, String),
    CardThumb(String, Option<String>),
    CardDeleted(String),
    /// Siblings in their new order.
    Order(Vec<String>),
    FolderGroup(String, String),
    GroupAdded(String),
    GroupRenamed(String, String),
    GroupDeleted(String),
}

fn apply_change(ops: &mut FolderOps, change: FolderChange) {
    match change {
        FolderChange::None => {}
        FolderChange::Created(p) => {
            ops.deleted.retain(|d| !under(&p, d));
            ops.created.push(p);
        }
        FolderChange::Renamed(old, new) => {
            ops.deleted.retain(|d| !under(&new, d));
            ops.renamed.push((old, new));
        }
        FolderChange::Deleted(p) => {
            ops.created.retain(|c| !under(c, &p));
            ops.deleted.push(p);
        }
        FolderChange::CardName(id, name) => {
            ops.card_names.insert(id, name);
        }
        FolderChange::CardFolder(id, folder) => {
            ops.deleted.retain(|d| !under(&folder, d));
            ops.card_folders.insert(id, folder);
        }
        FolderChange::CardDescription(id, text) => {
            ops.card_descriptions.insert(id, text);
        }
        FolderChange::CardThumb(id, path) => {
            ops.card_thumbs.insert(id, path);
        }
        FolderChange::CardDeleted(id) => {
            ops.cards_deleted.insert(id);
        }
        FolderChange::Order(names) => {
            for (i, n) in names.into_iter().enumerate() {
                ops.order.insert(n, i64::try_from(i).unwrap_or(i64::MAX));
            }
        }
        FolderChange::FolderGroup(folder, group) => {
            ops.folder_groups.insert(folder, group);
        }
        FolderChange::GroupAdded(name) => {
            ops.groups_deleted.retain(|g| *g != name);
            ops.groups_added.push(name);
        }
        FolderChange::GroupRenamed(old, new) => {
            for g in ops.folder_groups.values_mut() {
                if *g == old {
                    *g = new.clone();
                }
            }
            ops.groups_renamed.push((old, new));
        }
        FolderChange::GroupDeleted(name) => {
            for g in ops.folder_groups.values_mut() {
                if *g == name {
                    g.clear();
                }
            }
            ops.groups_added.retain(|g| *g != name);
            ops.groups_deleted.push(name);
        }
    }
}

/// Run a backend operation in the background; the status line says how it went.
fn spawn_folder_op(ctx: &egui::Context, op: &'static str, args: Value, change: FolderChange, done: String) {
    spawn_ml_op(ctx, move || backend_rpc(op, args), change, done);
}

/// Run a Motion Library operation in the background, then rebuild the index; the status line says
/// how it went.
pub(crate) fn spawn_ml_op(ctx: &egui::Context, op: impl FnOnce() -> Result<Value, String> + Send + 'static, change: FolderChange, done: String) {
    spawn_ml_op_with(ctx, op, change, move |_| done, true);
}

/// [`spawn_ml_op`] with a message made from the result, and the index rebuilt only when `reindex`
/// (operations that change no library file, e.g. using a card, don't).
pub(crate) fn spawn_ml_op_with(
    ctx: &egui::Context,
    op: impl FnOnce() -> Result<Value, String> + Send + 'static,
    change: FolderChange,
    done: impl FnOnce(&Value) -> String + Send + 'static,
    reindex: bool,
) {
    let ctx = ctx.clone();
    let run = move || {
        let result = op();
        let done = match &result {
            Ok(v) => done(v),
            Err(_) => String::new(),
        };
        {
            let mut ops = folder_ops();
            match &result {
                Ok(_) => {
                    apply_change(&mut ops, change);
                    ops.revision += 1;
                    ops.status = Some(if reindex { format!("{done} · updating the library…") } else { done.clone() });
                }
                Err(e) => ops.status = Some(e.clone()),
            }
        }
        ctx.request_repaint();
        if result.is_ok() && reindex {
            reindex_library();
            let mut ops = folder_ops();
            ops.revision += 1;
            ops.status = Some(done);
            drop(ops);
            ctx.request_repaint();
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(run);
    #[cfg(target_arch = "wasm32")]
    run();
}

/// `motionLibrary.folder`: `op` create (`category` = parent, `name`), rename (`category`, `name`),
/// setIcon (`category`, `path`; a file dialog without it), resetIcon, delete (`confirm`; else it
/// asks), contents, move (`category` into `parent`, "" = the top level), reorder (`category` before
/// or `after` its sibling `target`), group (`category`, `group`: "" = none; asks without it).
/// Without a `name`, create and rename ask for one in the panel.
pub fn folder_command(app: &mut dyn TimelineHost, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let op = params.get("op").and_then(Value::as_str).ok_or("missing `op`: create, rename, setIcon, resetIcon, delete, contents, move, reorder or group")?;
    let category = params.get("category").and_then(Value::as_str).unwrap_or_default().trim_matches('/').to_string();
    let name = params.get("name").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    if op != "create" && category.is_empty() {
        return Err("missing `category` (the folder path, e.g. `Devin Jatho/UI`)".into());
    }
    let leaf = category.rsplit('/').next().unwrap_or(&category).to_string();
    let ask = |app: &mut dyn TimelineHost, op: &str, text: String, contents: Value| {
        app.ui_mut().motion_library.folder_ask = Some(FolderAsk { op: op.to_string(), category: category.clone(), text, contents });
        Ok(json!({"dialog": "motionLibrary.folder"}))
    };
    if name.contains('/') {
        return Err("a folder name can't contain /".into());
    }
    match op {
        "create" => {
            if name.is_empty() {
                return ask(app, "create", String::new(), Value::Null);
            }
            let path = if category.is_empty() { name.clone() } else { format!("{category}/{name}") };
            app.ui_mut().status = format!("Creating “{name}”…");
            if !category.is_empty() {
                app.ui_mut().motion_library.expanded.insert(category.clone());
            }
            spawn_folder_op(ctx, "create_folder", json!({"category": path, "group": ""}), FolderChange::Created(path.clone()), format!("Created “{name}”"));
            Ok(json!({"category": path}))
        }
        "rename" => {
            if name.is_empty() {
                return ask(app, "rename", leaf, Value::Null);
            }
            let new = match category.rsplit_once('/') {
                Some((parent, _)) => format!("{parent}/{name}"),
                None => name.clone(),
            };
            app.ui_mut().status = format!("Renaming “{leaf}”…");
            let view = &mut app.ui_mut().motion_library;
            if under(&view.folder, &category) {
                view.folder = format!("{new}{}", &view.folder[category.len()..]);
            }
            spawn_folder_op(
                ctx,
                "rename_folder",
                json!({"category": category, "name": name}),
                FolderChange::Renamed(category.clone(), new.clone()),
                format!("Renamed to “{name}”"),
            );
            Ok(json!({"category": new}))
        }
        "setIcon" => {
            let Some(path) = params.get("path").and_then(Value::as_str) else {
                const IMAGES: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "svg", "heic"];
                if let Some(spawn) = app.app_ref().and_then(|a| a.hooks.file_dialog_async.as_ref()) {
                    let (ctx, category, leaf) = (ctx.clone(), category.clone(), leaf.clone());
                    spawn(
                        crate::FileDialog::Open { exts: IMAGES.iter().map(|e| e.to_string()).collect(), many: false },
                        Box::new(move |paths| {
                            if let Some(path) = paths.into_iter().next() {
                                spawn_folder_op(
                                    &ctx,
                                    "set_folder_icon",
                                    json!({"category": category, "path": path}),
                                    FolderChange::None,
                                    format!("New icon for “{leaf}”"),
                                );
                            }
                        }),
                    );
                    return Ok(json!({"dialog": "motionLibrary.folder"}));
                }
                let picked = app.pick_files(IMAGES).unwrap_or_default();
                let Some(path) = picked.into_iter().next() else { return Ok(Value::Null) };
                spawn_folder_op(ctx, "set_folder_icon", json!({"category": category, "path": path}), FolderChange::None, format!("New icon for “{leaf}”"));
                return Ok(json!({"category": category}));
            };
            spawn_folder_op(ctx, "set_folder_icon", json!({"category": category, "path": path}), FolderChange::None, format!("New icon for “{leaf}”"));
            Ok(json!({"category": category}))
        }
        "resetIcon" => {
            spawn_folder_op(ctx, "reset_folder_icon", json!({"category": category}), FolderChange::None, format!("Default icon for “{leaf}”"));
            Ok(json!({"category": category}))
        }
        "contents" => backend_rpc("folder_contents", json!({"category": category})),
        "move" => {
            // Into another folder (`parent`; "" = the top level).
            let parent = params.get("parent").and_then(Value::as_str).ok_or("missing `parent` (\"\" = the top level)")?.trim_matches('/').to_string();
            if under(&parent, &category) {
                return Err("a folder can't go inside itself".into());
            }
            let new = if parent.is_empty() { leaf.clone() } else { format!("{parent}/{leaf}") };
            if new == category {
                return Ok(json!({"category": new}));
            }
            app.ui_mut().status = format!("Moving “{leaf}”…");
            let view = &mut app.ui_mut().motion_library;
            if under(&view.folder, &category) {
                view.folder = format!("{new}{}", &view.folder[category.len()..]);
            }
            if !parent.is_empty() {
                view.expanded.insert(parent.clone());
            }
            let to = if parent.is_empty() { tr("the top level").to_string() } else { parent.clone() };
            spawn_folder_op(
                ctx,
                "move_folder",
                json!({"category": category, "parent": parent}),
                FolderChange::Renamed(category.clone(), new.clone()),
                format!("Moved “{leaf}” to {to}"),
            );
            Ok(json!({"category": new}))
        }
        "reorder" => {
            // Before or after a sibling (`target`, `after`).
            let target = params.get("target").and_then(Value::as_str).ok_or("missing `target` (a sibling folder)")?.trim_matches('/').to_string();
            let after = params.get("after").and_then(Value::as_bool).unwrap_or(false);
            let parent = category.rsplit_once('/').map_or("", |(p, _)| p).to_string();
            if target.rsplit_once('/').map_or("", |(p, _)| p) != parent {
                return Err("reorder moves a folder among its siblings: `target` has another parent".into());
            }
            let lib = local_library(ctx, &app.session().prefs.libraries);
            let before = library_siblings(&lib, &parent);
            let next = reordered(&before, &category, &target, after).ok_or("the folder or its target isn't in the library (refresh and drag again)")?;
            if next == before {
                return Ok(json!({"categories": next}));
            }
            spawn_folder_op(
                ctx,
                "reorder_folders",
                json!({"parent": parent, "categories": next}),
                FolderChange::Order(next.clone()),
                tr("Folder order saved").to_string(),
            );
            Ok(json!({"categories": next}))
        }
        "group" => {
            if category.contains('/') {
                return Err("only top folders (collections) belong to a group".into());
            }
            let Some(group) = params.get("group").and_then(Value::as_str).map(str::trim) else {
                let lib = local_library(ctx, &app.session().prefs.libraries);
                let names = group_names(&lib, &folder_ops());
                if names.is_empty() {
                    return Err("there are no collection groups yet: make one with New group…".into());
                }
                let mut options = vec![String::new()];
                options.extend(names);
                let current = folder_group(&lib, &folder_ops(), &category);
                let choice = options.iter().position(|g| *g == current).unwrap_or(0);
                app.ui_mut().motion_library.card_ask =
                    Some(CardAsk { op: "groupAssign".into(), id: category.clone(), name: category.clone(), options, choice, ..CardAsk::default() });
                return Ok(json!({"dialog": "motionLibrary.group"}));
            };
            let group = group.to_string();
            let label = if group.is_empty() { tr("no group").to_string() } else { format!("“{group}”") };
            spawn_folder_op(
                ctx,
                "set_folder_group",
                json!({"category": category, "group": group}),
                FolderChange::FolderGroup(category.clone(), group.clone()),
                format!("“{leaf}” is in {label}"),
            );
            Ok(json!({"category": category, "group": group}))
        }
        "delete" => {
            if params.get("confirm") != Some(&Value::Bool(true)) {
                let contents = backend_rpc("folder_contents", json!({"category": category}))?;
                return ask(app, "delete", String::new(), contents);
            }
            let contents = match params.get("expected") {
                Some(c) if c.is_object() => c.clone(),
                _ => backend_rpc("folder_contents", json!({"category": category}))?,
            };
            let args = json!({
                "category": category,
                "delete_contents": true,
                "expected_entry_ids": contents["entry_ids"],
                "expected_folders": contents["folders"],
            });
            app.ui_mut().status = format!("Deleting “{leaf}”…");
            let view = &mut app.ui_mut().motion_library;
            if under(&view.folder, &category) {
                view.folder.clear();
            }
            spawn_folder_op(ctx, "delete_folder", args, FolderChange::Deleted(category.clone()), format!("Deleted “{leaf}”"));
            Ok(json!({"deleted": category}))
        }
        other => Err(format!("unknown folder op `{other}` (create, rename, setIcon, resetIcon, delete, contents, move, reorder, group)")),
    }
}

/// The folder's right-click menu (the "All motion" row: New folder… only).
fn folder_menu(auto: &mut crate::automation::Registry, resp: &egui::Response, path: &str, danger: Color32, groups: bool) -> Option<(&'static str, Value)> {
    let mut pick = None;
    crate::widgets::context_menu(resp, |ui| {
        let mut entry = |ui: &mut egui::Ui, key: &str, label: &str, color: Option<Color32>, params: Value| {
            let text = egui::RichText::new(tr(label));
            let b = ui.add(egui::Button::new(match color {
                Some(c) => text.color(c),
                None => text,
            }));
            auto.add_response(&format!("motionLibrary.folderMenu.{key}"), &b, label);
            if b.clicked() {
                pick = Some(("motionLibrary.folder", params));
                ui.close();
            }
        };
        if path.is_empty() {
            entry(ui, "newFolder", "New folder…", None, json!({"op": "create", "category": ""}));
            return;
        }
        entry(ui, "newSubfolder", "New subfolder…", None, json!({"op": "create", "category": path}));
        entry(ui, "rename", "Rename…", None, json!({"op": "rename", "category": path}));
        if path.contains('/') {
            entry(ui, "moveTop", "Move to Top Level", None, json!({"op": "move", "category": path, "parent": ""}));
        } else if groups {
            entry(ui, "moveGroup", "Move to Group…", None, json!({"op": "group", "category": path}));
        }
        entry(ui, "chooseIcon", "Choose icon…", None, json!({"op": "setIcon", "category": path}));
        entry(ui, "resetIcon", "Restore default icon", None, json!({"op": "resetIcon", "category": path}));
        ui.separator();
        entry(ui, "delete", "Delete folder…", Some(danger), json!({"op": "delete", "category": path}));
    });
    pick
}

/// The folder dialog over the grid: the command to run when it's answered, `Some(None)` to close.
fn folder_ask_sheet(app: &mut dyn TimelineHost, ui: &mut egui::Ui, grid: Rect, ask: &mut FolderAsk, s: f32) -> Option<Option<Value>> {
    let t = app.tokens();
    let p = ui.painter().with_clip_rect(grid);
    let _ = ui.interact(grid, egui::Id::new("ml-folder-ask-block"), Sense::click_and_drag());
    p.rect_filled(grid, 0.0, Color32::from_black_alpha(120));
    let w = (grid.width() - 32.0 * s).clamp(120.0, 380.0 * s);
    let h = (grid.height() - 32.0 * s).clamp(90.0, 150.0 * s);
    let r = Rect::from_center_size(grid.center(), vec2(w, h));
    p.rect_filled(r, CornerRadius::same(10), t.header_bg);
    p.rect_stroke(r, CornerRadius::same(10), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    app.auto_mut().add("motionLibrary.folderAsk", r, &ask.op);
    let inner = r.shrink(14.0 * s);
    let leaf = ask.category.rsplit('/').next().unwrap_or(&ask.category).to_string();
    let (title, ok_label) = match ask.op.as_str() {
        "create" if ask.category.is_empty() => ("New folder".to_string(), "Create"),
        "create" => (format!("New folder in “{leaf}”"), "Create"),
        "rename" => (format!("Rename “{leaf}”"), "Rename"),
        _ => (format!("Delete “{leaf}”?"), "Delete"),
    };
    p.text(inner.min, Align2::LEFT_TOP, &title, Tokens::semibold(13.0 * s), t.text);
    let body_r = Rect::from_min_max(pos2(inner.min.x, inner.min.y + 26.0 * s), pos2(inner.max.x, inner.max.y - 32.0 * s));
    let enter = ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
    if ask.op == "delete" {
        let items = ask.contents["items"].as_u64().or_else(|| ask.contents["entry_ids"].as_array().map(|a| a.len() as u64)).unwrap_or(0);
        let subs = ask.contents["subfolders"].as_u64().or_else(|| ask.contents["folders"].as_array().map(|a| a.len().saturating_sub(1) as u64)).unwrap_or(0);
        let body = if items == 0 && subs == 0 {
            "The folder is empty.".to_string()
        } else {
            format!("This deletes {items} item(s) and {subs} subfolder(s) with it, and their files. It can't be undone.")
        };
        let galley = p.layout(body, Tokens::ui(11.5 * s), t.text_dim, body_r.width().max(40.0));
        p.with_clip_rect(body_r.intersect(grid)).galley(body_r.min, galley, t.text_dim);
    } else {
        let field = Rect::from_min_size(body_r.min, vec2(body_r.width(), 24.0 * s));
        let resp = ui
            .put(field, egui::TextEdit::singleline(&mut ask.text).hint_text(tr("Folder name")).vertical_align(egui::Align::Center).font(Tokens::ui(12.0 * s)));
        app.auto_mut().add_response("motionLibrary.folderAsk.name", &resp, "Folder name");
        if !resp.has_focus() && !resp.lost_focus() {
            resp.request_focus();
        }
    }
    let bw = 84.0 * s;
    let ok = Rect::from_min_size(pos2(inner.max.x - bw, inner.max.y - 24.0 * s), vec2(bw, 24.0 * s));
    let cancel = ok.translate(vec2(-(bw + 8.0 * s), 0.0));
    if kit::button(app, ui, cancel, "motionLibrary.folderAsk.cancel", "Cancel", false) || ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
        return Some(None);
    }
    let name = ask.text.trim();
    let valid = ask.op == "delete" || !name.is_empty() && !name.contains('/');
    if (kit::button(app, ui, ok, "motionLibrary.folderAsk.ok", ok_label, true) || enter && ask.op != "delete") && valid {
        let params = match ask.op.as_str() {
            "delete" => json!({"op": "delete", "category": ask.category, "confirm": true, "expected": ask.contents}),
            op => json!({"op": op, "category": ask.category, "name": name}),
        };
        return Some(Some(params));
    }
    None
}

// ── Cards, groups, imports (right-click a card, the group picker, files dropped on the panel) ──
//
// As in the browser panel (:8765): Rename…, Edit description…, Move to folder…, Choose
// thumbnail…, Restore default thumbnail, Generate preview, Delete card…, Ease cards' Edit /
// Duplicate / Reverse graph; collection groups; files dropped on the panel saved into a
// collection; Refresh syncing the library's sources. Library changes go through the backend
// (`backend.py rpc`), then the index is rebuilt; this run's changes show at once.

/// The library's folders (its `folders` table as the index has it, with this run's changes) under
/// `parent`, in the tree's order: the siblings a reorder lists.
fn library_siblings(lib: &LocalLib, parent: &str) -> Vec<String> {
    let ops = folder_ops();
    let mut names: Vec<String> = Vec::new();
    for f in lib.folders.iter().chain(ops.created.iter()) {
        let f = renamed_path(f.trim_matches('/'), &ops.renamed);
        if f.is_empty() || ops.deleted.iter().any(|d| under(&f, d)) || names.contains(&f) {
            continue;
        }
        if f.rsplit_once('/').map_or("", |(p, _)| p) == parent {
            names.push(f);
        }
    }
    let mut order = lib.order.clone();
    order.extend(ops.order.iter().map(|(k, v)| (k.clone(), *v)));
    let sorting = Sorting { order, priorities: lib.priorities.clone() };
    names.sort_by(|a, b| sorting.cmp(a, b, parent.is_empty()));
    names
}

/// `siblings` with `folder` moved before (or `after`) `target`; `None` when either isn't there.
fn reordered(siblings: &[String], folder: &str, target: &str, after: bool) -> Option<Vec<String>> {
    if folder == target || !siblings.iter().any(|f| f == folder) {
        return None;
    }
    let mut next: Vec<String> = siblings.iter().filter(|f| *f != folder).cloned().collect();
    let at = next.iter().position(|f| f == target)? + usize::from(after);
    next.insert(at.min(next.len()), folder.to_string());
    Some(next)
}

/// The collection groups (the library's and this run's), by name.
fn group_names(lib: &LocalLib, ops: &FolderOps) -> Vec<String> {
    let rename = |g: &str| ops.groups_renamed.iter().fold(g.to_string(), |g, (old, new)| if g == *old { new.clone() } else { g });
    let mut names: Vec<String> = lib
        .group_names
        .iter()
        .chain(lib.groups.values())
        .chain(ops.groups_added.iter())
        .chain(ops.folder_groups.values())
        .map(|g| rename(g))
        .filter(|g| !g.is_empty() && !ops.groups_deleted.contains(g))
        .collect();
    names.sort_by_key(|g| g.to_lowercase());
    names.dedup();
    names
}

/// The group of the collection `folder` is in (`""`: none).
fn folder_group(lib: &LocalLib, ops: &FolderOps, folder: &str) -> String {
    let top = folder.split('/').next().unwrap_or(folder);
    if let Some(g) = ops.folder_groups.get(top) {
        return g.clone();
    }
    let g = lib.groups.get(top).cloned().unwrap_or_default();
    let g = ops.groups_renamed.iter().fold(g, |g, (old, new)| if g == *old { new.clone() } else { g });
    if ops.groups_deleted.contains(&g) { String::new() } else { g }
}

/// A card as the panel shows it: this Mac's index and this run's edits over the published card;
/// `None` when it was deleted here (or its folder was).
fn overlay_card(card: &Card, lib: &LocalLib, ops: &FolderOps, published: bool) -> Option<Card> {
    if ops.cards_deleted.contains(&card.id) {
        return None;
    }
    let mut c = card.clone();
    match lib.cards.get(&c.id) {
        Some(l) => {
            if !l.name.is_empty() {
                c.name = l.name.clone();
            }
            if !l.category.is_empty() {
                c.category = l.category.clone();
            }
        }
        // A published card this Mac's complete index no longer lists: deleted here.
        None if published && lib.complete => return None,
        None => {}
    }
    if let Some(n) = ops.card_names.get(&c.id) {
        c.name = n.clone();
    }
    if let Some(f) = ops.card_folders.get(&c.id) {
        c.category = f.clone();
    }
    if let Some(d) = ops.card_descriptions.get(&c.id) {
        c.description = if c.description.is_empty() { d.clone() } else { format!("{d} · {}", c.description) };
    }
    c.category = renamed_path(&c.category, &ops.renamed);
    if ops.deleted.iter().any(|d| under(&c.category, d)) {
        return None;
    }
    Some(c)
}

/// Every card the panel lists: the catalog's and this Mac's unpublished ones, with this Mac's
/// edits (cached per catalog, index and edit).
fn library_cards(ctx: &egui::Context, client: &Client, local: &Arc<LocalIndex>, lib: &Arc<LocalLib>) -> Arc<Vec<Card>> {
    type Cached = ((u64, usize, usize, u64), Arc<Vec<Card>>);
    let id = egui::Id::new("ml-library-cards");
    let key = (generation(client), Arc::as_ptr(local) as usize, Arc::as_ptr(lib) as usize, folder_ops().revision);
    if let Some((k, cards)) = ctx.data(|d| d.get_temp::<Cached>(id))
        && k == key
    {
        return cards;
    }
    let local_only = local_only_cards(ctx, client, local);
    let cat = client.catalog();
    let ops = folder_ops();
    let mut cards: Vec<Card> = cat.iter().flat_map(|c| c.cards.iter()).filter_map(|c| overlay_card(c, lib, &ops, true)).collect();
    cards.extend(local_only.iter().filter_map(|c| overlay_card(c, lib, &ops, false)));
    // Cards only this Mac's library has and EffectCraft can't open from files (Ease, Expression,
    // unconverted After Effects comps…, the index's `catalog_only`): listed so the plug applies them.
    let listed: BTreeSet<String> = cards.iter().map(|c| c.id.clone()).chain(local.values().map(|e| e.id.clone())).collect();
    let unpublished: Vec<Card> = lib.cards.iter().filter(|(id, _)| !listed.contains(id.as_str())).map(|(id, l)| unpublished_card(id, l)).collect();
    cards.extend(unpublished.iter().filter_map(|c| overlay_card(c, lib, &ops, false)));
    drop(ops);
    let cards = Arc::new(cards);
    ctx.data_mut(|d| d.insert_temp::<Cached>(id, (key, cards.clone())));
    cards
}

/// A card of the local library that isn't published, as the panel lists it (on this Mac only).
fn unpublished_card(id: &str, l: &LocalCard) -> Card {
    Card {
        id: id.to_string(),
        name: l.name.clone(),
        category: l.category.clone(),
        kind: if l.kind_label == "Comp" { "comp" } else { "footage" }.into(),
        kind_label: l.kind_label.clone(),
        description: l.description.clone(),
        on_mac: true,
        ..Card::default()
    }
}

/// Cards per folder, each counted in its folder and every folder above it.
fn folder_counts<'a>(cards: impl Iterator<Item = &'a Card>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for c in cards {
        let mut path = c.category.as_str();
        loop {
            *counts.entry(path.to_string()).or_insert(0) += 1;
            match path.rsplit_once('/') {
                Some((parent, _)) => path = parent,
                None => break,
            }
        }
    }
    counts
}

/// How this Mac uses a card EffectCraft can't apply from files, through the Motion Library's
/// EffectCraft plug (as the web panel does): `ease` (onto the selected keyframes), `expression`
/// (onto the selected property), `comp` (converts its After Effects project, then nests it),
/// `layers` (converts it, then copies the saved layers into the active comp).
fn plug_use(card: &Card, local: &LocalIndex, lib: &LocalLib) -> Option<&'static str> {
    if usable(card, local) || !ml_backend().is_some_and(|p| p.is_file()) {
        return None;
    }
    let l = lib.cards.get(&card.id)?;
    match kind_label(card) {
        "Ease" => Some("ease"),
        "Expression" => Some("expression"),
        "Comp" if l.app != "effectcraft" && card.on_mac => Some("comp"),
        "Layers" if l.app != "effectcraft" && card.on_mac => Some("layers"),
        _ => None,
    }
}

/// A card dialog over the panel: a name or description to type, a folder or group to choose, a
/// delete to confirm.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CardAsk {
    /// rename, describe, move, delete, groupNew, groupRename, groupDelete, groupAssign
    pub op: String,
    /// The card (card dialogs), the group (groupRename, groupDelete), the collection (groupAssign).
    pub id: String,
    /// What the title names.
    pub name: String,
    pub text: String,
    /// move: folders; groupAssign: groups (`""` = none).
    pub options: Vec<String>,
    pub choice: usize,
}

/// The card dialog over the grid: the command to run when it's answered, `Some(None)` to close.
fn card_ask_sheet(app: &mut dyn TimelineHost, ui: &mut egui::Ui, grid: Rect, ask: &mut CardAsk, s: f32) -> Option<Option<(&'static str, Value)>> {
    let t = app.tokens();
    let p = ui.painter().with_clip_rect(grid);
    let _ = ui.interact(grid, egui::Id::new("ml-card-ask-block"), Sense::click_and_drag());
    p.rect_filled(grid, 0.0, Color32::from_black_alpha(120));
    let multiline = ask.op == "describe";
    let w = (grid.width() - 32.0 * s).clamp(120.0, 420.0 * s);
    let h = (grid.height() - 32.0 * s).clamp(90.0, if multiline { 280.0 } else { 160.0 } * s);
    let r = Rect::from_center_size(grid.center(), vec2(w, h));
    p.rect_filled(r, CornerRadius::same(10), t.header_bg);
    p.rect_stroke(r, CornerRadius::same(10), Stroke::new(1.0, t.separator), egui::StrokeKind::Inside);
    app.auto_mut().add("motionLibrary.cardAsk", r, &ask.op);
    let inner = r.shrink(14.0 * s);
    let (title, ok_label, body) = match ask.op.as_str() {
        "rename" => (tr("Rename card").to_string(), "Rename", ""),
        "describe" => (format!("{} · {}", tr("Description"), ask.name), "Save", "When to use it, what it does, its limits. Searchable, and agents read it."),
        "move" => (format!("Move “{}” to", ask.name), "Move", ""),
        "delete" => (
            format!("Delete “{}”?", ask.name),
            "Delete",
            "This removes the card, its saved versions and its library previews. Original projects and source media stay untouched.",
        ),
        "groupNew" => (tr("New group").to_string(), "Create", "A group collects collections (top folders). Your collections stay where they are."),
        "groupRename" => (format!("Rename group “{}”", ask.name), "Rename", ""),
        "groupDelete" => (format!("Delete group “{}”?", ask.name), "Delete", "The group label is removed. Its collections and cards stay in All collections."),
        _ => (format!("Move “{}” to group", ask.name), "Move", ""),
    };
    let title = elide(&p, &title, &Tokens::semibold(13.0 * s), inner.width());
    p.text(inner.min, Align2::LEFT_TOP, &title, Tokens::semibold(13.0 * s), t.text);
    let mut y = inner.min.y + 24.0 * s;
    if !body.is_empty() {
        let galley = p.layout(tr(body).to_string(), Tokens::ui(11.5 * s), t.text_dim, inner.width().max(40.0));
        let gh = galley.size().y;
        p.galley(pos2(inner.min.x, y), galley, t.text_dim);
        y += gh + 8.0 * s;
    }
    let bottom = inner.max.y - 32.0 * s;
    let enter = ui.ctx().input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
    match ask.op.as_str() {
        "rename" | "groupNew" | "groupRename" | "describe" => {
            let field = Rect::from_min_max(pos2(inner.min.x, y), pos2(inner.max.x, if multiline { bottom } else { (y + 24.0 * s).min(bottom) }));
            let edit = if multiline {
                egui::TextEdit::multiline(&mut ask.text).hint_text(tr("Use for… Behavior… Adjustable controls… Avoid when…"))
            } else {
                egui::TextEdit::singleline(&mut ask.text).hint_text(tr(if ask.op == "rename" { "Card name" } else { "Group name" }))
            };
            let resp = ui.put(field, edit);
            app.auto_mut().add_response("motionLibrary.cardAsk.text", &resp, &ask.op);
            if !resp.has_focus() && !resp.lost_focus() {
                resp.request_focus();
            }
        }
        "move" | "groupAssign" => {
            let labels: Vec<String> = ask.options.iter().map(|o| if o.is_empty() { tr("No group").to_string() } else { o.clone() }).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let dr = Rect::from_min_size(pos2(inner.min.x, y), vec2(inner.width(), 24.0 * s));
            if let Some(i) = kit::dropdown(app, ui, dr, "motionLibrary.cardAsk.choice", &refs, ask.choice) {
                ask.choice = i;
            }
        }
        _ => {}
    }
    let bw = 84.0 * s;
    let ok = Rect::from_min_size(pos2(inner.max.x - bw, inner.max.y - 24.0 * s), vec2(bw, 24.0 * s));
    let cancel = ok.translate(vec2(-(bw + 8.0 * s), 0.0));
    if kit::button(app, ui, cancel, "motionLibrary.cardAsk.cancel", "Cancel", false) || ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
        return Some(None);
    }
    let text = ask.text.trim().to_string();
    let needs_text = matches!(ask.op.as_str(), "rename" | "groupNew" | "groupRename");
    let valid = !needs_text || !text.is_empty();
    let submit = kit::button(app, ui, ok, "motionLibrary.cardAsk.ok", ok_label, true) || (enter && !multiline && ask.op != "delete" && ask.op != "groupDelete");
    if !submit || !valid {
        return None;
    }
    let pick = |i: usize| ask.options.get(i).cloned().unwrap_or_default();
    Some(Some(match ask.op.as_str() {
        "rename" => ("motionLibrary.card", json!({"op": "rename", "id": ask.id, "name": text})),
        "describe" => ("motionLibrary.card", json!({"op": "describe", "id": ask.id, "description": ask.text.trim()})),
        "move" => ("motionLibrary.card", json!({"op": "move", "id": ask.id, "category": pick(ask.choice)})),
        "delete" => ("motionLibrary.card", json!({"op": "delete", "id": ask.id, "confirm": true})),
        "groupNew" => ("motionLibrary.group", json!({"op": "create", "name": text})),
        "groupRename" => ("motionLibrary.group", json!({"op": "rename", "name": ask.id, "newName": text})),
        "groupDelete" => ("motionLibrary.group", json!({"op": "delete", "name": ask.id, "confirm": true})),
        _ => ("motionLibrary.folder", json!({"op": "group", "category": ask.id, "group": pick(ask.choice)})),
    }))
}

/// An Ease card's two saved keyframes as a normalized cubic Bézier `[x1, y1, x2, y2]` (the web
/// panel's `CurveMath.fromKeys`); `None` when they aren't two numeric keys.
pub(crate) fn keys_curve(keys: &Value) -> Option<[f64; 4]> {
    let (a, b) = (keys.get(0)?, keys.get(1)?);
    let (ta, tb, va, vb) = (a["time"].as_f64()?, b["time"].as_f64()?, a["value"].as_f64()?, b["value"].as_f64()?);
    let (dt, dv) = (tb - ta, vb - va);
    if dt.is_nan() || dt <= 0.0 || dv.abs() < 1e-8 || !dt.is_finite() || !dv.is_finite() {
        return None;
    }
    let out = a["out_ease"].get(0);
    let inc = b["in_ease"].get(0);
    let x1 = out.and_then(|o| o["influence"].as_f64()).map_or(1.0 / 3.0, |i| (i / 100.0).clamp(0.001, 1.0));
    let x2 = inc.and_then(|o| o["influence"].as_f64()).map_or(2.0 / 3.0, |i| 1.0 - (i / 100.0).clamp(0.001, 1.0));
    let y1 = match out.and_then(|o| o["speed"].as_f64()) {
        Some(sp) if a["out_type"] != "LINEAR" => sp * dt * x1 / dv,
        _ => x1,
    };
    let y2 = match inc.and_then(|o| o["speed"].as_f64()) {
        Some(sp) if b["in_type"] != "LINEAR" => 1.0 - sp * dt * (1.0 - x2) / dv,
        _ => x2,
    };
    [x1, y1, x2, y2].iter().all(|v| v.is_finite()).then_some([x1, y1, x2, y2])
}

/// The same ease played backwards in time.
pub(crate) fn reverse_curve(c: [f64; 4]) -> [f64; 4] {
    [1.0 - c[2], 1.0 - c[3], 1.0 - c[0], 1.0 - c[1]]
}

/// Speed along a cubic Bézier ease (0 → 1) at 81 points: `(x, speed)`, as the web panel's speed
/// graph samples it.
fn speed_samples(c: [f64; 4]) -> Vec<(f64, f64)> {
    let [x1, y1, x2, y2] = c;
    let d = |t: f64, p: f64, q: f64| 3.0 * (1.0 - t) * (1.0 - t) * p + 6.0 * (1.0 - t) * t * (q - p) + 3.0 * t * t * (1.0 - q);
    (0..=80)
        .map(|i| {
            let t = f64::from(i) / 80.0;
            let x = 3.0 * (1.0 - t) * (1.0 - t) * t * x1 + 3.0 * (1.0 - t) * t * t * x2 + t * t * t;
            (x, d(t, y1, y2) / d(t, x1, x2).max(1e-5))
        })
        .collect()
}

/// An Ease card's speed graph drawn in `r`.
fn paint_speed_graph(p: &egui::Painter, r: Rect, curve: [f64; 4], color: Color32, grid: Color32) {
    let samples = speed_samples(curve);
    let (mut lo, mut hi) = samples.iter().fold((0.0f64, 0.0f64), |(lo, hi), (_, v)| (lo.min(*v), hi.max(*v)));
    let pad = (hi - lo).max(1e-6) * 0.12;
    lo -= pad;
    hi += pad;
    let inner = r.shrink2(vec2(r.width() * 0.06, r.height() * 0.12));
    for f in [0.25f32, 0.5, 0.75] {
        let y = inner.min.y + inner.height() * f;
        p.line_segment([pos2(inner.min.x, y), pos2(inner.max.x, y)], Stroke::new(1.0, grid));
    }
    let pts: Vec<egui::Pos2> =
        samples.iter().map(|(x, v)| pos2(inner.min.x + *x as f32 * inner.width(), inner.min.y + ((hi - v) / (hi - lo)) as f32 * inner.height())).collect();
    p.add(egui::Shape::line(pts, Stroke::new(2.0, color)));
}

/// The card's details from the library (`describe`), fetched in the background when its details
/// open: `(card, result)`; `None` inside while it loads.
type Described = Option<(String, Option<Result<Value, String>>)>;
static DESCRIBED: Mutex<Described> = Mutex::new(None);

/// Start fetching card `id`'s details (once per card) and return them when they are in.
fn described(ctx: &egui::Context, id: &str) -> Option<Result<Value, String>> {
    let mut slot = DESCRIBED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cur, result)) = slot.as_ref()
        && cur == id
    {
        return result.clone();
    }
    if !ml_backend().is_some_and(|p| p.is_file()) {
        return None;
    }
    *slot = Some((id.to_string(), None));
    drop(slot);
    let (ctx, id) = (ctx.clone(), id.to_string());
    let run = move || {
        let r = backend_rpc("describe", json!({"id": id})).map(|v| if v.get("entry").is_some() { v["entry"].clone() } else { v });
        let mut slot = DESCRIBED.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|(cur, _)| *cur == id) {
            *slot = Some((id, Some(r)));
        }
        drop(slot);
        ctx.request_repaint();
    };
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::spawn(run);
    #[cfg(target_arch = "wasm32")]
    run();
    None
}

/// Fetch the open card's details again (after an edit).
fn forget_described() {
    *DESCRIBED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The library's own name for an index state (the web panel's `label`).
fn index_label(status: &str) -> &str {
    match status {
        "catalog-only" | "" => "Not indexed",
        "partial-motion-index" => "Partial motion index",
        "native-key-index" | "saved-key-index" => "Keys & expressions indexed",
        "partial-index" => "Partially indexed",
        "structure-only" => "Structure indexed",
        "source-changed" => "Source changed",
        "indexed" => "Motion indexed",
        "indexing" => "Indexing",
        "stale" => "Needs refresh",
        "failed" => "Index failed",
        "ready" => "Ready",
        other => other,
    }
}

/// `motionLibrary.card`: `op` use (an Ease, Expression or After Effects comp card, through the
/// EffectCraft plug), rename (`name`), describe (`description`), move (`category`), setThumbnail
/// (`path`: an image, GIF or video; a file dialog without it), resetThumbnail, preview (render its
/// preview again), delete (`confirm`), duplicateGraph, reverseGraph, editGraph (Ease cards), details
/// (the library's details, fetched in the background). Without their value, rename, describe, move
/// and delete ask in the panel.
pub fn card_command(app: &mut dyn TimelineHost, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let op = params.get("op").and_then(Value::as_str).ok_or(
        "missing `op`: use, rename, describe, move, setThumbnail, resetThumbnail, preview, delete, duplicateGraph, reverseGraph, editGraph or details",
    )?;
    let id = params.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("missing `id` (a Motion Library card id)")?.to_string();
    if id.contains(['/', '\\']) || id.contains("..") {
        return Err(format!("`{id}` isn't a card id"));
    }
    let lib = local_library(ctx, &app.session().prefs.libraries);
    let local = app.session().motion_library_local();
    let shown = app.session().motion_library().ok().and_then(|client| library_cards(ctx, &client, &local, &lib).iter().find(|c| c.id == id).cloned());
    let edit = lib.cards.get(&id).cloned();
    if op == "details" {
        app.ui_mut().motion_library.details = Some(id.clone());
        app.ui_mut().motion_library.selected = Some(id.clone());
        forget_described();
        return Ok(json!({"details": id}));
    }
    let name = shown.as_ref().map(|c| c.name.clone()).or_else(|| edit.as_ref().map(|e| e.name.clone())).ok_or(format!("no Motion Library card `{id}`"))?;
    // Everything else edits this Mac's library.
    let Some(edit) = edit else {
        return Err(format!("“{name}” isn't in this computer's Motion Library (only the Mac that keeps the library edits its cards)"));
    };
    let category = {
        let ops = folder_ops();
        ops.card_folders.get(&id).cloned().unwrap_or_else(|| renamed_path(&edit.category, &ops.renamed))
    };
    let text = |k: &str| params.get(k).and_then(Value::as_str).map(str::trim).map(str::to_string);
    let ask = |app: &mut dyn TimelineHost, ask: CardAsk| {
        app.ui_mut().motion_library.card_ask = Some(ask);
        Ok(json!({"dialog": "motionLibrary.card"}))
    };
    match op {
        "use" => {
            let card = shown.ok_or(format!("no Motion Library card `{id}`"))?;
            let how = plug_use(&card, &local, &lib).ok_or(format!("“{name}”: {}", mac_words(&local).1))?;
            let port = control_port(app)?;
            // A drop places it at the drop time: the plug inserts at the playhead.
            if let Some(t) = params.get("time").and_then(Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0) {
                app.session_mut().execute("time.set", json!({"time": t})).map_err(|e| e.to_string())?;
            }
            let converts = matches!(how, "comp" | "layers");
            let (plug_op, args) = if converts { ("insert", json!({"id": id, "mode": "comp"})) } else { ("use_asset", json!({"id": id})) };
            app.ui_mut().status = if converts { format!("Converting “{name}” from After Effects…") } else { format!("Applying “{name}”…") };
            let done = match how {
                "ease" => format!("Applied the ease “{name}” to the selected keyframes"),
                "expression" => format!("Applied the expression “{name}”"),
                "layers" => format!("Added the layers of “{name}”"),
                _ => format!("Inserted “{name}”"),
            };
            spawn_ml_op_with(ctx, move || plug_rpc(port, plug_op, args), FolderChange::None, move |_| done, false);
            Ok(json!({"using": id, "as": how}))
        }
        "rename" => {
            let Some(new) = text("name").filter(|n| !n.is_empty()) else {
                return ask(app, CardAsk { op: "rename".into(), id, text: name.clone(), name, ..CardAsk::default() });
            };
            if new.chars().count() > 240 || new.chars().any(char::is_control) {
                return Err("a card name is 1 to 240 characters, without control characters".into());
            }
            if new == name {
                return Ok(json!({"name": new}));
            }
            app.ui_mut().status = format!("Renaming “{name}”…");
            spawn_folder_op(
                ctx,
                "rename_entry",
                json!({"id": id, "name": new, "expected_name": edit.name}),
                FolderChange::CardName(id.clone(), new.clone()),
                format!("Renamed to “{new}”"),
            );
            Ok(json!({"id": id, "name": new}))
        }
        "describe" => {
            let current = folder_ops().card_descriptions.get(&id).cloned().unwrap_or_else(|| edit.description.clone());
            let Some(description) = params.get("description").and_then(Value::as_str).map(str::trim).map(str::to_string) else {
                return ask(app, CardAsk { op: "describe".into(), id, text: current, name, ..CardAsk::default() });
            };
            if description.chars().count() > 10_000 {
                return Err("a description is at most 10 000 characters".into());
            }
            spawn_folder_op(
                ctx,
                "set_description",
                json!({"id": id, "description": description, "expected_description": current}),
                FolderChange::CardDescription(id.clone(), description.clone()),
                format!("Description of “{name}” saved"),
            );
            forget_described();
            Ok(json!({"id": id, "description": description}))
        }
        "move" => {
            let Some(to) = text("category").map(|c| c.trim_matches('/').to_string()).filter(|c| !c.is_empty()) else {
                let client = app.session().motion_library()?;
                let tree = folder_tree(ctx, &client, &local, &lib, "");
                let options: Vec<String> = tree.iter().map(|r| r.path.clone()).collect();
                if options.is_empty() {
                    return Err("there are no folders to move it to".into());
                }
                let choice = options.iter().position(|o| *o == category).unwrap_or(0);
                return ask(app, CardAsk { op: "move".into(), id, name, options, choice, ..CardAsk::default() });
            };
            if to == category {
                return Ok(json!({"id": id, "category": to}));
            }
            app.ui_mut().status = format!("Moving “{name}”…");
            spawn_folder_op(
                ctx,
                "move",
                json!({"id": id, "category": to}),
                FolderChange::CardFolder(id.clone(), to.clone()),
                format!("Moved “{name}” to {to}"),
            );
            Ok(json!({"id": id, "category": to}))
        }
        "setThumbnail" => {
            const MEDIA: &[&str] =
                &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif", "gif", "mp4", "mov", "m4v", "webm", "avi", "mkv"];
            let set = move |ctx: &egui::Context, path: String, id: String, name: String| {
                spawn_folder_op(
                    ctx,
                    "set_thumbnail",
                    json!({"id": id, "path": path}),
                    FolderChange::CardThumb(id.clone(), Some(path.clone())),
                    format!("New thumbnail for “{name}”"),
                );
            };
            if let Some(path) = text("path").filter(|p| !p.is_empty()) {
                set(ctx, path.clone(), id.clone(), name);
                return Ok(json!({"id": id, "thumbnail": path}));
            }
            if let Some(spawn) = app.app_ref().and_then(|a| a.hooks.file_dialog_async.as_ref()) {
                let ctx2 = ctx.clone();
                spawn(
                    crate::FileDialog::Open { exts: MEDIA.iter().map(|e| e.to_string()).collect(), many: false },
                    Box::new(move |paths| {
                        if let Some(path) = paths.into_iter().next() {
                            set(&ctx2, path, id, name);
                        }
                    }),
                );
                return Ok(json!({"dialog": "motionLibrary.card"}));
            }
            let picked = app.pick_files(MEDIA).unwrap_or_default();
            let Some(path) = picked.into_iter().next() else { return Ok(Value::Null) };
            set(ctx, path.clone(), id.clone(), name);
            Ok(json!({"id": id, "thumbnail": path}))
        }
        "resetThumbnail" => {
            spawn_folder_op(ctx, "reset_thumbnail", json!({"id": id}), FolderChange::CardThumb(id.clone(), None), format!("Default thumbnail for “{name}”"));
            Ok(json!({"id": id}))
        }
        "preview" => {
            if edit.app != "effectcraft" {
                return Err(format!("“{name}” was saved in After Effects: its preview renders from the Motion Library panel there"));
            }
            let port = control_port(app).ok();
            app.ui_mut().status = format!("Rendering a preview of “{name}”…");
            let id2 = id.clone();
            spawn_ml_op(
                ctx,
                move || {
                    let r = ml_rpc("effectcraft_plug.py", &[], port, "preview", json!({"id": id2}))?;
                    wait_for_preview(&id2);
                    Ok(r)
                },
                FolderChange::None,
                format!("Preview of “{name}” ready"),
            );
            Ok(json!({"id": id, "preview": "queued"}))
        }
        "delete" => {
            if params.get("confirm") != Some(&Value::Bool(true)) {
                return ask(app, CardAsk { op: "delete".into(), id, name, ..CardAsk::default() });
            }
            let view = &mut app.ui_mut().motion_library;
            if view.selected.as_deref() == Some(id.as_str()) {
                view.selected = None;
            }
            if view.details.as_deref() == Some(id.as_str()) {
                view.details = None;
            }
            let expected_name = folder_ops().card_names.get(&id).cloned().unwrap_or_else(|| edit.name.clone());
            app.ui_mut().status = format!("Deleting “{name}”…");
            spawn_folder_op(
                ctx,
                "delete_entry",
                json!({"id": id, "confirm_delete": true, "expected_name": expected_name, "expected_category": category}),
                FolderChange::CardDeleted(id.clone()),
                format!("Deleted “{name}”"),
            );
            Ok(json!({"deleted": id}))
        }
        "duplicateGraph" | "reverseGraph" | "editGraph" if edit.kind_label != "Ease" => Err(format!("“{name}” isn't an Ease card")),
        "duplicateGraph" => {
            spawn_ml_op_with(
                ctx,
                move || backend_rpc("duplicate_interpolation", json!({"id": id})),
                FolderChange::None,
                |v| format!("Duplicated the graph as “{}”", v["name"].as_str().unwrap_or("a copy")),
                true,
            );
            Ok(json!({"duplicating": name}))
        }
        "reverseGraph" => {
            let id2 = id.clone();
            spawn_ml_op(
                ctx,
                move || {
                    let entry = backend_rpc("describe", json!({"id": id2}))?;
                    let entry = if entry.get("entry").is_some() { entry["entry"].clone() } else { entry };
                    let keys = entry["payload"]["keys"].clone();
                    let curve = keys_curve(&keys).ok_or("this graph isn't two numeric keyframes")?;
                    backend_rpc("update_interpolation", json!({"id": id2, "expected_keys": keys, "curve": reverse_curve(curve)}))
                },
                FolderChange::None,
                format!("Reversed the graph “{name}”"),
            );
            forget_described();
            Ok(json!({"reversing": id}))
        }
        "editGraph" => app.invoke(ctx, "motionLibrary.curve", json!({"op": "editCard", "id": id})),
        other => Err(format!("unknown card op `{other}`")),
    }
}

/// `motionLibrary.group`: `op` choose (`name`: the group shown, "" = all collections), create
/// (`name`), rename (`name`, `newName`), delete (`name`, `confirm`), list. Without their names,
/// create, rename and delete ask in the panel.
pub fn group_command(app: &mut dyn TimelineHost, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let op = params.get("op").and_then(Value::as_str).ok_or("missing `op`: choose, create, rename, delete or list")?;
    let name = params.get("name").and_then(Value::as_str).map(str::trim).unwrap_or_default().to_string();
    let lib = local_library(ctx, &app.session().prefs.libraries);
    let names = group_names(&lib, &folder_ops());
    let known = |n: &str| names.iter().any(|g| g == n);
    match op {
        "list" => Ok(json!({"groups": names, "current": app.ui_mut().motion_library.group})),
        "choose" => {
            if !name.is_empty() && !known(&name) {
                return Err(format!("no collection group `{name}` ({})", names.join(", ")));
            }
            let view = &mut app.ui_mut().motion_library;
            view.group = name.clone();
            view.folder.clear();
            view.scroll = 0.0;
            view.popover.clear();
            Ok(json!({"group": name}))
        }
        "create" => {
            if name.is_empty() {
                app.ui_mut().motion_library.popover.clear();
                app.ui_mut().motion_library.card_ask = Some(CardAsk { op: "groupNew".into(), ..CardAsk::default() });
                return Ok(json!({"dialog": "motionLibrary.group"}));
            }
            if names.iter().any(|g| g.eq_ignore_ascii_case(&name)) {
                return Err(format!("a group named “{name}” already exists"));
            }
            app.ui_mut().motion_library.group = name.clone();
            app.ui_mut().motion_library.folder.clear();
            spawn_folder_op(
                ctx,
                "create_group",
                json!({"name": name}),
                FolderChange::GroupAdded(name.clone()),
                format!("Group “{name}” created: add a collection with + or move one here"),
            );
            Ok(json!({"group": name}))
        }
        "rename" => {
            if !known(&name) {
                return Err(format!("no collection group `{name}`"));
            }
            let Some(new) = params.get("newName").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).map(str::to_string) else {
                app.ui_mut().motion_library.popover.clear();
                app.ui_mut().motion_library.card_ask =
                    Some(CardAsk { op: "groupRename".into(), id: name.clone(), text: name.clone(), name, ..CardAsk::default() });
                return Ok(json!({"dialog": "motionLibrary.group"}));
            };
            if app.ui_mut().motion_library.group == name {
                app.ui_mut().motion_library.group = new.clone();
            }
            spawn_folder_op(
                ctx,
                "rename_group",
                json!({"name": name, "new_name": new}),
                FolderChange::GroupRenamed(name.clone(), new.clone()),
                format!("Renamed the group to “{new}”"),
            );
            Ok(json!({"group": new}))
        }
        "delete" => {
            if !known(&name) {
                return Err(format!("no collection group `{name}`"));
            }
            if params.get("confirm") != Some(&Value::Bool(true)) {
                app.ui_mut().motion_library.popover.clear();
                app.ui_mut().motion_library.card_ask = Some(CardAsk { op: "groupDelete".into(), id: name.clone(), name, ..CardAsk::default() });
                return Ok(json!({"dialog": "motionLibrary.group"}));
            }
            if app.ui_mut().motion_library.group == name {
                app.ui_mut().motion_library.group.clear();
            }
            spawn_folder_op(
                ctx,
                "delete_group",
                json!({"name": name}),
                FolderChange::GroupDeleted(name.clone()),
                format!("Group “{name}” removed: its collections and cards were kept"),
            );
            Ok(json!({"deleted": name}))
        }
        other => Err(format!("unknown group op `{other}` (choose, create, rename, delete, list)")),
    }
}

/// `motionLibrary.importFiles`: save files (`paths`) as cards of collection `category` (default the
/// folder open in the panel, else "My motion"), as files dropped on the web panel are.
pub fn import_files_command(app: &mut dyn TimelineHost, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let paths: Vec<String> = match params.get("paths") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    };
    if paths.is_empty() {
        return Err("missing `paths` (files to save in the library)".into());
    }
    let view = &app.ui_mut().motion_library;
    let category = params
        .get("category")
        .and_then(Value::as_str)
        .map(|c| c.trim_matches('/').to_string())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| if view.folder.is_empty() { "My motion".to_string() } else { view.folder.clone() });
    app.ui_mut().status = format!("Saving {} file{} in {category}…", paths.len(), if paths.len() == 1 { "" } else { "s" });
    let n = paths.len();
    spawn_ml_op_with(
        ctx,
        move || backend_rpc("import_files", json!({"paths": paths, "category": category})),
        FolderChange::None,
        move |r| {
            let saved = r["saved"].as_array().map_or(0, Vec::len);
            let mut msg = format!("Saved {saved} of {n} file(s) in {}", r["category"].as_str().unwrap_or("the library"));
            let errors: Vec<String> = r["errors"]
                .as_array()
                .map(|e| {
                    e.iter()
                        .take(3)
                        .map(|x| {
                            let path = x["path"].as_str().unwrap_or_default();
                            format!("{}: {}", path.rsplit('/').next().unwrap_or(path), x["error"].as_str().unwrap_or_default())
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !errors.is_empty() {
                msg.push_str(" · ");
                msg.push_str(&errors.join("; "));
            }
            msg
        },
        true,
    );
    Ok(json!({"saving": n}))
}

/// `motionLibrary.refresh`: check the published catalog (`library.sync`) and, on the Mac with the
/// library, sync its sources (EarnEdits, SceneYard, SFX) and media previews, then rebuild the index
/// (the web panel's Refresh).
pub fn refresh_command(app: &mut dyn TimelineHost, ctx: &egui::Context, _params: &Value) -> Result<Value, String> {
    let _ = app.session_mut().execute("library.sync", json!({"wait": false}));
    let lib = local_library(ctx, &app.session().prefs.libraries);
    if !lib.present() || !ml_backend().is_some_and(|p| p.is_file()) {
        return Ok(json!({"sources": false}));
    }
    app.ui_mut().status = tr("Refreshing the library's sources…").to_string();
    spawn_ml_op_with(
        ctx,
        || {
            let mut errors = vec![];
            for op in ["sync_earnedits", "sync_sceneyard", "sync_sfx"] {
                if let Err(e) = backend_rpc(op, json!({})) {
                    errors.push(format!("{op}: {e}"));
                }
            }
            match backend_rpc("prepare_media", json!({})) {
                Ok(m) => {
                    if let Some(n) = m["errors"].as_array().map(Vec::len).filter(|n| *n > 0) {
                        errors.push(format!("{n} media previews could not be refreshed"));
                    }
                }
                Err(e) => errors.push(e),
            }
            Ok(json!({"errors": errors}))
        },
        FolderChange::None,
        |r| match r["errors"].as_array().filter(|e| !e.is_empty()) {
            Some(e) => format!("Library refreshed · {}", e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")),
            None => tr("Library sources refreshed").to_string(),
        },
        true,
    );
    Ok(json!({"sources": true}))
}

#[cfg(test)]
mod library_tests {
    use super::*;

    #[test]
    fn drop_between_a_group_and_the_next_row_stays_a_sibling() {
        // Refs dragged to the line under Backgrounds (the top of Graphs): after Backgrounds, not out.
        let (to, place) = boundary_drop("Devin Jatho/Refs", "Graphs", DropPlace::Before, Some("Devin Jatho/Backgrounds"));
        assert_eq!((to.as_str(), place), ("Devin Jatho/Backgrounds", DropPlace::After));
        assert_eq!(folder_drop("Devin Jatho/Refs", &to, place).1["op"], "reorder");
        // Under an open subfolder: after its sibling ancestor.
        let (to, _) = boundary_drop("Devin Jatho/Refs", "Graphs", DropPlace::Before, Some("Devin Jatho/Icons/Instagram"));
        assert_eq!(to, "Devin Jatho/Icons");
        // A top folder dropped at the top of a group's first child still moves in.
        let (to, place) = boundary_drop("Graphs", "Devin Jatho/Elements", DropPlace::Before, Some("Devin Jatho"));
        assert_eq!((to.as_str(), place), ("Devin Jatho/Elements", DropPlace::Before));
        // Its own row above: unchanged.
        let (to, _) = boundary_drop("Devin Jatho/Refs", "Graphs", DropPlace::Before, Some("Devin Jatho/Refs"));
        assert_eq!(to, "Graphs");
    }

    #[test]
    fn reorder_moves_before_and_after() {
        let s: Vec<String> = ["A", "B", "C", "D"].iter().map(|x| x.to_string()).collect();
        assert_eq!(reordered(&s, "D", "B", false).unwrap(), ["A", "D", "B", "C"]);
        assert_eq!(reordered(&s, "A", "C", true).unwrap(), ["B", "C", "A", "D"]);
        assert_eq!(reordered(&s, "A", "D", true).unwrap(), ["B", "C", "D", "A"]);
        assert!(reordered(&s, "A", "A", false).is_none());
        assert!(reordered(&s, "X", "A", false).is_none());
        assert!(reordered(&s, "A", "X", false).is_none());
    }

    #[test]
    fn library_order_then_priorities_then_names() {
        let mut order = BTreeMap::new();
        order.insert("Zeta".to_string(), 0);
        order.insert("Alpha".to_string(), 1);
        let s = Sorting { order, priorities: vec!["Devin Jatho".into()] };
        let mut top = vec!["Beta", "Alpha", "Devin Jatho", "Zeta"];
        top.sort_by(|a, b| s.cmp(a, b, true));
        assert_eq!(top, ["Zeta", "Alpha", "Devin Jatho", "Beta"]);
        let catalog = vec![
            FolderRow { path: "B".into(), name: "B".into(), depth: 0, cards: 3, has_children: false, icon: None },
            FolderRow { path: "A".into(), name: "A".into(), depth: 0, cards: 1, has_children: false, icon: None },
        ];
        let ops = folder_ops_fresh();
        let mut counts = BTreeMap::new();
        counts.insert("A".to_string(), 7);
        counts.insert("C/D".to_string(), 2);
        counts.insert("C".to_string(), 2);
        let rows = merge_folders(&catalog, &[], &ops, &["C/D".to_string()], Some(&counts), Some(&Sorting::default()));
        let got: Vec<(&str, usize)> = rows.iter().map(|r| (r.path.as_str(), r.cards)).collect();
        assert_eq!(got, [("A", 7), ("B", 0), ("C", 2), ("C/D", 2)]);
        // Without the library's order: the catalog's.
        let rows = merge_folders(&catalog, &[], &ops, &[], None, None);
        assert_eq!(rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["B", "A"]);
    }

    fn folder_ops_fresh() -> FolderOps {
        FolderOps {
            created: vec![],
            deleted: vec![],
            renamed: vec![],
            card_names: BTreeMap::new(),
            card_folders: BTreeMap::new(),
            card_descriptions: BTreeMap::new(),
            card_thumbs: BTreeMap::new(),
            cards_deleted: BTreeSet::new(),
            order: BTreeMap::new(),
            folder_groups: BTreeMap::new(),
            groups_added: vec![],
            groups_renamed: vec![],
            groups_deleted: vec![],
            revision: 0,
            status: None,
        }
    }

    #[test]
    fn overlay_renames_moves_and_deletes() {
        let lib = parse_local_lib(&[json!({
            "collections": {"folders": ["A"], "groups": {"A": "Client"}, "group_names": ["Client", "Empty"]},
            "entries": [{"id": "c1", "name": "Renamed", "category": "A/B", "raw_description": "mine", "type_label": "Comp"}],
            "catalog_only": [{"id": "e1", "name": "Snappy", "category": "Graphs", "type_label": "Ease",
                              "speed_graph": [{"time": 0, "value": 0}, {"time": 1, "value": 1}]}]
        })]);
        assert!(lib.complete && lib.present());
        assert_eq!(lib.cards["c1"].description, "mine");
        assert!(lib.cards["e1"].speed_graph.is_some());
        // An unpublished Ease card is listed (on this Mac) with its label, for the plug to apply.
        let e = unpublished_card("e1", &lib.cards["e1"]);
        assert_eq!((e.name.as_str(), e.category.as_str(), e.kind_label.as_str(), e.on_mac), ("Snappy", "Graphs", "Ease", true));
        let mut ops = folder_ops_fresh();
        let published = Card { id: "c1".into(), name: "Old".into(), category: "X".into(), ..Default::default() };
        let c = overlay_card(&published, &lib, &ops, true).unwrap();
        assert_eq!((c.name.as_str(), c.category.as_str()), ("Renamed", "A/B"));
        // A published card the complete index doesn't list was deleted here.
        let gone = Card { id: "zz".into(), ..Default::default() };
        assert!(overlay_card(&gone, &lib, &ops, true).is_none());
        assert!(overlay_card(&gone, &lib, &ops, false).is_some());
        apply_change(&mut ops, FolderChange::CardFolder("c1".into(), "Z".into()));
        apply_change(&mut ops, FolderChange::CardName("c1".into(), "New".into()));
        let c = overlay_card(&published, &lib, &ops, true).unwrap();
        assert_eq!((c.name.as_str(), c.category.as_str()), ("New", "Z"));
        apply_change(&mut ops, FolderChange::Renamed("Z".into(), "Y".into()));
        assert_eq!(overlay_card(&published, &lib, &ops, true).unwrap().category, "Y");
        apply_change(&mut ops, FolderChange::Deleted("Y".into()));
        assert!(overlay_card(&published, &lib, &ops, true).is_none());
        // Groups: the index's, renamed, deleted.
        assert_eq!(group_names(&lib, &ops), ["Client", "Empty"]);
        assert_eq!(folder_group(&lib, &ops, "A/B"), "Client");
        apply_change(&mut ops, FolderChange::GroupRenamed("Client".into(), "Work".into()));
        assert_eq!(folder_group(&lib, &ops, "A"), "Work");
        apply_change(&mut ops, FolderChange::GroupDeleted("Work".into()));
        assert_eq!(folder_group(&lib, &ops, "A"), "");
        assert_eq!(group_names(&lib, &ops), ["Empty"]);
    }

    #[test]
    fn sfx_volume_defaults_persists_and_clamps() {
        let v = ViewState::default();
        assert_eq!(v.sfx_volume, DEFAULT_SFX_VOLUME);
        // A workspace saved before the setting existed keeps the default.
        let old: ViewState = serde_json::from_value(json!({"query": "hit", "cardSize": 144.0})).unwrap();
        assert_eq!(old.sfx_volume, DEFAULT_SFX_VOLUME);
        // Kept with the workspace.
        let mut v = ViewState { sfx_volume: 0.65, ..ViewState::default() };
        let back: ViewState = serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
        assert_eq!(back.sfx_volume, 0.65);
        // `motionLibrary.view {sfxVolume}`, clamped; a bad value is refused.
        assert_eq!(set_view(&mut v, &json!({"sfxVolume": 0.4})).unwrap()["sfxVolume"], json!(0.4f32));
        set_view(&mut v, &json!({"sfxVolume": 3})).unwrap();
        assert_eq!(v.sfx_volume, 1.0);
        assert!(set_view(&mut v, &json!({"sfxVolume": "loud"})).is_err());
        v.sfx_volume = f32::NAN;
        v.clamp();
        assert_eq!(v.sfx_volume, DEFAULT_SFX_VOLUME);
    }

    #[test]
    fn sound_cards_play_local_files_first_and_only_small_bucket_files() {
        use effectcraft_engine::motionlib::{DirRemote, DirStore, sha256_hex};
        let dir = std::env::temp_dir().join(format!("ec-ml-sound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("remote/blobs")).unwrap();
        let file = dir.join("Hit.wav");
        std::fs::write(&file, b"RIFF").unwrap();
        let client = Client::new(Arc::new(DirRemote::new(dir.join("remote"))), Arc::new(DirStore::new(dir.join("cache"))));
        let ctx = egui::Context::default();
        let path = |p: &std::path::Path| p.to_string_lossy().to_string();
        // This Mac's library folder: its preview when it exists, else its file.
        let mut local = LocalIndex::new();
        let entry = effectcraft_engine::motion_library::LocalEntry {
            id: "sfx-1".into(),
            kind: "audio".into(),
            file: path(&file),
            preview: path(&dir.join("gone/preview.mp3")),
            ..Default::default()
        };
        local.insert("sfx-1".into(), entry);
        let card = Card { id: "sfx-1".into(), kind: "mac".into(), on_mac: true, ..Card::default() };
        assert!(is_sound(&card, &local));
        assert!(matches!(sound_source(&ctx, &client, &local, &card), SoundSource::File(p) if p == path(&file)));
        // Not a sound: no hover sound (its preview plays as video instead).
        let comp = Card { id: "c".into(), kind: "comp".into(), kind_label: "Comp".into(), ..Card::default() };
        assert!(!is_sound(&comp, &local));
        // Elsewhere: the bucket's copy, when small enough to fetch for a hover.
        let bytes = b"RIFF small sound".to_vec();
        let hash = sha256_hex(&bytes);
        std::fs::write(dir.join("remote/blobs").join(&hash), &bytes).unwrap();
        let small = BlobRef { hash, size: bytes.len() as u64, gz: false, name: "Whoosh.wav".into() };
        let remote = Card { id: "sfx-2".into(), kind: "footage".into(), kind_label: "Sound".into(), project: Some(small), ..Card::default() };
        assert!(is_sound(&remote, &local));
        let t = std::time::Instant::now();
        let got = loop {
            match sound_source(&ctx, &client, &local, &remote) {
                SoundSource::Loading if t.elapsed() < std::time::Duration::from_secs(10) => std::thread::sleep(std::time::Duration::from_millis(5)),
                other => break other,
            }
        };
        assert!(matches!(got, SoundSource::File(p) if std::fs::read(&p).unwrap() == bytes));
        // A big file isn't downloaded for a hover.
        let big = BlobRef { hash: "ab".repeat(32), size: SOUND_HOVER_MAX_BYTES + 1, gz: false, name: "Long.wav".into() };
        let huge = Card { id: "sfx-3".into(), kind: "footage".into(), kind_label: "Sound".into(), project: Some(big), ..Card::default() };
        assert!(matches!(sound_source(&ctx, &client, &local, &huge), SoundSource::None));
        assert!(!dir.join("cache").read_dir().map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().contains("abab"))).unwrap_or(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ease_keys_to_curve_and_back() {
        // Influence 75 % out, 50 % in, zero speeds: a strong ease out, gentle ease in.
        let keys = json!([
            {"time": 0.0, "value": 0.0, "out_ease": [{"speed": 0.0, "influence": 75.0}], "out_type": "BEZIER"},
            {"time": 1.0, "value": 100.0, "in_ease": [{"speed": 0.0, "influence": 50.0}], "in_type": "BEZIER"}
        ]);
        let c = keys_curve(&keys).unwrap();
        assert!((c[0] - 0.75).abs() < 1e-9 && c[1].abs() < 1e-9 && (c[2] - 0.5).abs() < 1e-9 && (c[3] - 1.0).abs() < 1e-9);
        assert_eq!(reverse_curve(c), [0.5, 0.0, 0.25, 1.0]);
        assert!(keys_curve(&json!([{"time": 1, "value": 0}, {"time": 1, "value": 1}])).is_none());
        assert!(keys_curve(&json!([{"time": 0, "value": 3}, {"time": 1, "value": 3}])).is_none());
        assert!(keys_curve(&json!("x")).is_none());
        // Linear: a constant speed.
        let lin = speed_samples([1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0]);
        assert!(lin.iter().all(|(_, v)| (v - 1.0).abs() < 1e-6));
    }
}

/// The settings store file the panel's display settings are kept in between sessions.
pub const DISPLAY_FILE: &str = "motion_library.json";

/// The part of the view kept between sessions: scale, card sizes, columns, the folder open and
/// its tree (not the search, the selection or the scroll).
fn kept(view: &ViewState) -> ViewState {
    ViewState { query: String::new(), selected: None, scroll: 0.0, folder_scroll: 0.0, ..view.clone() }
}

/// The display settings saved in an earlier session (a file that doesn't read is ignored). Run
/// once when the app starts.
pub(crate) fn load_display(app: &mut EffectcraftApp) {
    let text = app.session.config.as_ref().and_then(|c| c.read(DISPLAY_FILE));
    match text.map(|t| serde_json::from_str::<ViewState>(&t)) {
        Some(Ok(mut v)) => {
            v.clamp();
            app.ui.motion_library = v;
        }
        Some(Err(e)) => log::warn!("{DISPLAY_FILE}: {e}"),
        None => {}
    }
    app.motion_library_saved = Some(kept(&app.ui.motion_library));
}

/// Keep the display settings for the next session once they changed (not in the middle of a
/// drag). Run every frame.
pub(crate) fn store_display(app: &mut EffectcraftApp, ctx: &egui::Context) {
    if ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    let now = kept(&app.ui.motion_library);
    if app.motion_library_saved.as_ref() == Some(&now) {
        return;
    }
    if let Some(c) = &app.session.config {
        let written = serde_json::to_string(&now).map_err(std::io::Error::other).and_then(|t| c.write(DISPLAY_FILE, &t));
        if let Err(e) = written {
            log::warn!("{DISPLAY_FILE}: {e}");
        }
    }
    app.motion_library_saved = Some(now);
}

// ── Text animation presets (batch 1b, CX8, CX8c) ──────────────────────────────────────────────

/// The frontend half of `ml.saveTextPreset`: save the workshop layer's effects and text animators
/// (the engine selected them) as a preset card (the plug's `save_asset` kind `preset`, which runs
/// anim.savePreset on the selection and renders the layer as the preview; CX8c); cards of the same
/// name in that folder are deleted once the new one is saved, so saving again updates the preset.
pub fn save_text_preset(app: &mut dyn TimelineHost, ctx: &egui::Context, params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).ok_or("ml.saveTextPreset: no name")?.to_string();
    let category = params.get("category").and_then(Value::as_str).unwrap_or("Elias/Text animations").trim_matches('/').to_string();
    // The cards it replaces: the same name in the same folder.
    let old: Vec<(String, String)> = match app.session().motion_library() {
        Ok(client) => {
            let local = app.session().motion_library_local();
            let lib = local_library(ctx, &app.session().prefs.libraries);
            library_cards(ctx, &client, &local, &lib)
                .iter()
                .filter(|c| c.category == category && c.name.trim().eq_ignore_ascii_case(&name))
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect()
        }
        Err(_) => vec![],
    };
    let port = control_port(app)?;
    let args = json!({"kind": "preset", "name": name, "category": category, "description": "Text animation preset (from the Creating Text Presets comp)"});
    app.ui_mut().status = format!("Saving text preset “{name}” to {category}…");
    let replaced = old.len();
    let done = if replaced > 0 { format!("Updated text preset “{name}” in {category}") } else { format!("Saved text preset “{name}” to {category}") };
    let cat2 = category.clone();
    let save = move || {
        let r = ml_rpc("effectcraft_plug.py", &[], Some(port), "save_asset", args)?;
        let new_id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        wait_for_preview(&new_id);
        // delete_entry with the arguments the card menu's Delete sends.
        for (id, old_name) in old {
            if id != new_id {
                backend_rpc("delete_entry", json!({"id": id, "confirm_delete": true, "expected_name": old_name, "expected_category": cat2}))?;
            }
        }
        Ok(r)
    };
    spawn_ml_op(ctx, save, FolderChange::Created(category.clone()), done);
    Ok(json!({"saving": name, "category": category, "replaces": replaced}))
}
