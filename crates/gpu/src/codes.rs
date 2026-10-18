//! 8 bpc working images as their 8-bit codes (`Rgba8Uint`, a quarter of the f32 bytes).
//!
//! An 8 bpc project rounds every value to the 8-bit grid after each source, effect and layer
//! (`ops::quantize`, as After Effects holds 8-bit buffers there). Where an image is on that grid
//! the compositor can keep the integer codes instead of the floats: the floats are exactly
//! `k as f32 * (1.0 / 255.0)` (the CPU's rounding), so storing `k` loses nothing.
//!
//! - [`CodeImage`] is such an image. It is **not** a [`GpuImage`]: no float kernel can bind it.
//! - [`Pixels`] is what the compositor walk carries (layer sources, retained images): float or
//!   codes. The code-aware compositor passes take it; anything else asks for [`Pixels::float`]
//!   (an explicit decode, the same pass and floats as VRAM1's unpack), so a consumer that would
//!   read codes as floats is a type error, not a wrong pixel.
//! - The kernels reading codes are variants of `kernels.wgsl`'s compositor entry points, built
//!   from the same text: the entry points are extracted by name, `src` / `aux` / `out` retyped,
//!   and each texel decoded before any interpolation through a 256-entry table holding the CPU's
//!   own floats (a table load can't be reassociated by Metal's fast math, a multiply can). The
//!   f32 kernels are untouched. A pattern that no longer matches fails the variant's build: the
//!   context then has no codes bank and renders in f32 as before.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::context::{Enc, GpuImage, Params, init_resource};

/// The 8 bpc grid's levels (`ops::quantize`'s `levels` for 8 bpc).
pub(crate) const LEVELS: f32 = 255.0;

/// The texture format of [`CodeImage`]s.
pub(crate) const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Uint;

/// Profiling builds: `EFFECTCRAFT_Q8=0` keeps every working image f32 (A/B in one binary).
pub(crate) fn enabled() -> bool {
    if !cfg!(feature = "profile") {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("EFFECTCRAFT_Q8").as_deref() != Ok("0"))
}

/// The floats of the 256 codes, as the CPU rounds them (`k as f32 * (1.0 / 255.0)`, the value
/// `ops::quantize` and the CPU's `quantize` store).
pub(crate) fn decode_table() -> [f32; 256] {
    let inv = 1.0 / LEVELS;
    std::array::from_fn(|k| k as f32 * inv)
}

/// An image of 8 bpc codes (each channel `k` stands for `decode_table()[k]`; always on the 8 bpc
/// grid). Cheap to clone; immutable once written, like a [`GpuImage`].
#[derive(Clone, Debug)]
pub(crate) struct CodeImage(GpuImage);

impl CodeImage {
    /// A codes texture made elsewhere (packed retained images, packed uploads); `None` unless it
    /// is `Rgba8Uint`.
    pub(crate) fn wrap(img: GpuImage) -> Option<CodeImage> {
        (img.texture.format() == FORMAT).then_some(CodeImage(img))
    }
    pub(crate) fn width(&self) -> u32 {
        self.0.width
    }
    pub(crate) fn height(&self) -> u32 {
        self.0.height
    }
    pub(crate) fn share(&self) {
        self.0.share();
    }
    pub(crate) fn allocation(&self) -> Option<&std::sync::Arc<effectcraft_render::allocation::GpuAllocation>> {
        self.0.allocation.as_ref()
    }
    /// The codes texture, for copies and readbacks of the raw codes (never a float binding).
    pub(crate) fn texture(&self) -> &wgpu::Texture {
        &self.0.texture
    }
    pub(crate) fn view(&self) -> &wgpu::TextureView {
        &self.0.view
    }
    /// Whether `e` may draw on it in place ([`Enc::owns`]).
    pub(crate) fn owned_by(&self, e: &Enc) -> bool {
        e.owns(&self.0)
    }
    /// Bytes of the codes.
    pub(crate) fn bytes(&self) -> usize {
        (self.0.width as usize).saturating_mul(self.0.height as usize).saturating_mul(4)
    }
}

/// A working image of the compositor walk: f32, or 8 bpc codes.
#[derive(Clone, Debug)]
pub(crate) enum Pixels {
    Float(GpuImage),
    Codes(CodeImage),
}

impl From<GpuImage> for Pixels {
    fn from(img: GpuImage) -> Pixels {
        Pixels::Float(img)
    }
}

impl Pixels {
    pub(crate) fn width(&self) -> u32 {
        match self {
            Pixels::Float(g) => g.width,
            Pixels::Codes(c) => c.width(),
        }
    }
    pub(crate) fn height(&self) -> u32 {
        match self {
            Pixels::Float(g) => g.height,
            Pixels::Codes(c) => c.height(),
        }
    }
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width(), self.height())
    }
    /// The grid the values are on (see `GpuImage::levels`): codes are always on the 8 bpc one.
    pub(crate) fn levels(&self) -> Option<f32> {
        match self {
            Pixels::Float(g) => g.levels,
            Pixels::Codes(_) => Some(LEVELS),
        }
    }
    pub(crate) fn is_codes(&self) -> bool {
        matches!(self, Pixels::Codes(_))
    }
    /// The image as floats, for a consumer that reads only floats: codes are decoded into a new
    /// working image of `e` (bit-identical to the f32 image they came from). `None`: the decode
    /// kernel is missing (codes never exist without it).
    #[track_caller]
    pub(crate) fn float(&self, e: &mut Enc) -> Option<GpuImage> {
        match self {
            Pixels::Float(g) => Some(g.clone()),
            Pixels::Codes(c) => {
                if let Some(bank) = e.g.codes.as_ref() {
                    bank.promotions.fetch_add(1, Relaxed);
                    let at = std::panic::Location::caller();
                    bank.count_site(format!("{}:{}", at.file().rsplit('/').next().unwrap_or(""), at.line()));
                }
                decode(e, c)
            }
        }
    }
    fn view(&self) -> &wgpu::TextureView {
        match self {
            Pixels::Float(g) => &g.view,
            Pixels::Codes(c) => c.view(),
        }
    }
}

/// Which bindings of a compositor pass hold codes (the rest f32).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Io {
    pub src: bool,
    pub aux: bool,
    pub out: bool,
}

impl Io {
    fn index(self) -> usize {
        usize::from(self.src) | usize::from(self.aux) << 1 | usize::from(self.out) << 2
    }
    fn of(index: usize) -> Io {
        Io { src: index & 1 != 0, aux: index & 2 != 0, out: index & 4 != 0 }
    }
}

/// A compiled variant: its bind group layout and pipelines by entry point.
struct Variant {
    layout: wgpu::BindGroupLayout,
    pipes: Vec<(&'static str, wgpu::ComputePipeline)>,
}

/// The codes kernels of a device (built on first use, or by [`Bank::warm`]).
pub(crate) struct Bank {
    device: wgpu::Device,
    lut: wgpu::Buffer,
    /// By [`Io::index`] (index 0, all f32, is the ordinary kernels: never built here).
    variants: [OnceLock<Result<Variant, String>>; 8],
    /// The viewer's `display` pass reading codes.
    display: OnceLock<Result<Variant, String>>,
    /// Effect kernels reading codes: [`FX_SRC`] by `src`, [`FX_AUX`] by `aux` (f32 out).
    fx: [OnceLock<Result<Variant, String>>; 2],
    /// Codes decoded for a float-only consumer (diagnostics, tests).
    pub(crate) promotions: AtomicU64,
    /// Passes that read or wrote codes (diagnostics, tests).
    pub(crate) dispatches: AtomicU64,
    /// Decodes by the call site that asked for floats (diagnostics: `codes_stats`).
    pub(crate) sites: std::sync::Mutex<std::collections::BTreeMap<String, u64>>,
}

impl Bank {
    fn count_site(&self, site: String) {
        if let Ok(mut m) = self.sites.lock() {
            let n = m.entry(site).or_insert(0);
            *n = n.saturating_add(1);
        }
    }
    /// `Err` when the adapter can't store `Rgba8Uint` working textures.
    pub(crate) fn new(adapter: &wgpu::Adapter, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Bank, String> {
        let uses = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST;
        if !adapter.get_texture_format_features(FORMAT).allowed_usages.contains(uses) {
            return Err("no Rgba8Uint working textures".into());
        }
        let table = decode_table();
        let lut = init_resource(device, "8 bpc decode table", || {
            crate::context::buffer_init(device, queue, Some("8 bpc decode table"), bytemuck::cast_slice(&table), wgpu::BufferUsages::STORAGE)
        })?;
        Ok(Bank {
            device: device.clone(),
            lut,
            variants: Default::default(),
            display: OnceLock::new(),
            fx: Default::default(),
            promotions: AtomicU64::new(0),
            dispatches: AtomicU64::new(0),
            sites: Default::default(),
        })
    }

    /// Build every variant now (a startup worker), so no frame waits for a compile.
    pub(crate) fn warm(&self) -> Result<(), String> {
        for i in 1..8 {
            if let Err(e) = self.variant(Io::of(i)) {
                return Err(e.clone());
            }
        }
        for aux in [false, true] {
            if let Err(e) = self.fx_variant(aux) {
                return Err(e.clone());
            }
        }
        self.display_variant().map(|_| ()).map_err(Clone::clone)
    }

    /// The effect kernels reading codes by `aux` (else by `src`).
    fn fx_variant(&self, aux: bool) -> Result<&Variant, &String> {
        let cell = self.fx.get(usize::from(aux)).ok_or(&NO_VARIANT)?;
        cell.get_or_init(|| build_fx(&self.device, aux)).as_ref()
    }

    /// The pipeline of `entry` for `io`: a compositor variant, else (codes in, f32 out) an
    /// effect variant.
    fn find(&self, entry: &str, io: Io) -> Option<(&Variant, &wgpu::ComputePipeline)> {
        fn pick<'v>(v: &'v Variant, entry: &str) -> Option<&'v wgpu::ComputePipeline> {
            v.pipes.iter().find(|(name, _)| *name == entry).map(|(_, p)| p)
        }
        if let Ok(v) = self.variant(io)
            && let Some(pipe) = pick(v, entry)
        {
            return Some((v, pipe));
        }
        let fx = match io {
            Io { src: true, aux: false, out: false } => false,
            Io { src: false, aux: true, out: false } => true,
            _ => return None,
        };
        let v = self.fx_variant(fx).ok()?;
        pick(v, entry).map(|pipe| (v, pipe))
    }

    fn variant(&self, io: Io) -> Result<&Variant, &String> {
        let cell = self.variants.get(io.index()).ok_or(&NO_VARIANT)?;
        cell.get_or_init(|| build(&self.device, io)).as_ref()
    }

    fn display_variant(&self) -> Result<&Variant, &String> {
        self.display.get_or_init(|| build_display(&self.device)).as_ref()
    }

    /// Whether kernel `entry` exists for `io` (a producer asks before choosing codes output).
    pub(crate) fn has(&self, entry: &str, io: Io) -> bool {
        io.index() == 0 || self.find(entry, io).is_some()
    }
}

static NO_VARIANT: String = String::new();

/// Run compositor kernel `entry` over `groups` workgroups, reading `src` / `aux` and writing
/// `out`, each f32 or codes. All f32 is the ordinary kernel. Without the variant (it failed to
/// build), codes inputs are decoded and the ordinary kernel runs; codes output then can't be
/// written and the pass is reported missing (see `Enc::take_missing`): callers ask
/// [`Bank::has`] before choosing codes output.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch(
    e: &mut Enc,
    entry: &'static str,
    p: &Params,
    src: &Pixels,
    aux: Option<&Pixels>,
    out: &Pixels,
    data: Option<&wgpu::Buffer>,
    groups: (u32, u32),
) {
    let io = Io { src: src.is_codes(), aux: aux.is_some_and(Pixels::is_codes), out: out.is_codes() };
    if io.index() != 0
        && let Some(bank) = e.g.codes.as_ref()
        && let Some((v, pipe)) = bank.find(entry, io)
    {
        bank.dispatches.fetch_add(1, Relaxed);
        let views = [src.view(), aux.map_or(&e.g.dummy.view, Pixels::view), out.view()];
        e.dispatch_views(entry, pipe, &v.layout, p, views, data, groups, None, Some(&bank.lut));
        return;
    }
    let Pixels::Float(out) = out else {
        log::error!("gpu: no 8 bpc codes kernel {entry}");
        e.mark_missing();
        return;
    };
    if io.index() != 0
        && let Some(bank) = e.g.codes.as_ref()
    {
        bank.count_site(format!("no variant {entry}"));
    }
    let (Some(src), aux) = (src.float(e), aux.map(|a| a.float(e))) else {
        e.mark_missing();
        return;
    };
    let aux = match aux {
        Some(Some(a)) => Some(a),
        Some(None) => {
            e.mark_missing();
            return;
        }
        None => None,
    };
    e.dispatch(entry, p, &src, aux.as_ref(), out, data, groups);
}

/// Codes decoded into a new working image of `e` (the floats of [`decode_table`]); `None`
/// without the decode kernel.
pub(crate) fn decode(e: &mut Enc, c: &CodeImage) -> Option<GpuImage> {
    let bank = e.g.codes.as_ref()?;
    if !bank.has("decode", Io { src: true, aux: false, out: false }) {
        return None;
    }
    let mut out = e.scratch(c.width(), c.height());
    pixels(e, "decode", &Params::default(), &Pixels::Codes(c.clone()), None, &Pixels::Float(out.clone()), None);
    out.levels = Some(LEVELS);
    Some(out)
}

/// `c` inside a border of `pad` transparent pixels (`Buf::pad` on codes: code 0 is 0.0), as a
/// new codes image of `e`; `None` when it can't be made (over the size limit, no kernel).
pub(crate) fn pad(e: &mut Enc, c: &CodeImage, pad: u32) -> Option<CodeImage> {
    let w = c.width().checked_add(pad.checked_mul(2)?)?;
    let h = c.height().checked_add(pad.checked_mul(2)?)?;
    if !e.g.fits(w, h) || !e.g.codes.as_ref()?.has("clear", Io { src: false, aux: false, out: true }) {
        return None;
    }
    let out = Pixels::Codes(e.scratch_codes(w, h)?);
    pixels(e, "clear", &Params::default(), &Pixels::Float(e.g.dummy.clone()), None, &out, None);
    let Pixels::Codes(out) = out else { return None };
    e.copy_into(&c.0, &out.0, pad, pad);
    Some(out)
}

/// [`dispatch`] over `out`'s size (16 × 16 workgroups).
pub(crate) fn pixels(e: &mut Enc, entry: &'static str, p: &Params, src: &Pixels, aux: Option<&Pixels>, out: &Pixels, data: Option<&wgpu::Buffer>) {
    let groups = (out.width().div_ceil(16), out.height().div_ceil(16));
    dispatch(e, entry, p, src, aux, out, data, groups);
}

/// The codes `display` pass (`display.wgsl`'s `display`, each texel decoded first): its pipeline,
/// bind group layout (1: codes, 3: the RGBA8 level, 6: the table) and the table; `None` when
/// it is missing (the caller decodes and runs the f32 pass).
pub(crate) fn display_parts(g: &crate::context::GpuContext) -> Option<(&wgpu::ComputePipeline, &wgpu::BindGroupLayout, &wgpu::Buffer)> {
    let bank = g.codes.as_ref()?;
    let v = bank.display_variant().ok()?;
    let (_, pipe) = v.pipes.first()?;
    bank.dispatches.fetch_add(1, Relaxed);
    Some((pipe, &v.layout, &bank.lut))
}

// ---------------------------------------------------------------- shader variants

const COMMON: &str = include_str!("shaders/common.wgsl");
const KERNELS: &str = include_str!("shaders/kernels.wgsl");
const DISPLAY: &str = include_str!("shaders/display.wgsl");

/// The decode table and its lookup, shared by every variant.
const DECODE: &str = r#"
// 8 bpc codes → the CPU's floats (`codes::decode_table`).
@group(0) @binding(6) var<storage, read> lut: array<f32, 256>;
fn dec(k: vec4<u32>) -> vec4<f32> {
    return vec4<f32>(lut[min(k.x, 255u)], lut[min(k.y, 255u)], lut[min(k.z, 255u)], lut[min(k.w, 255u)]);
}
"#;

/// Codes to their floats (for a consumer that reads only floats: [`Pixels::float`]).
const DECODE_ENTRY: &str = r#"
@compute @workgroup_size(16, 16)
fn decode(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, textureLoad(src, p, 0));
}
"#;

/// `kernels.wgsl`'s `quantize_px` producing the codes instead of their floats: the same
/// arithmetic (alpha first, colour clamped to the quantised alpha `ac * inv`), without the final
/// `* inv` (decoding `k` gives that float back).
const STORE_CODES: &str = r#"
fn quantize_codes(s: vec4<f32>, levels: f32, inv: f32) -> vec4<u32> {
    let ac = floor(clamp(s.w, 0.0, 1.0) * levels + 0.5);
    let a = ac * inv;
    let r = floor(clamp(s.x, 0.0, a) * levels + 0.5);
    let g = floor(clamp(s.y, 0.0, a) * levels + 0.5);
    let b = floor(clamp(s.z, 0.0, a) * levels + 0.5);
    return vec4<u32>(vec4<f32>(r, g, b, ac));
}

// Store a compositing result as 8 bpc codes (`levels` is always 255 here).
fn store_q(p: vec2<i32>, v: vec4<f32>, levels: f32, inv: f32) {
    textureStore(out, p, quantize_codes(v, levels, inv));
}
"#;

/// A codes image cleared to code 0 (transparent black), for [`pad`].
const CLEAR_ENTRY: &str = r#"
@compute @workgroup_size(16, 16)
fn clear(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = out_dims();
    let p = vec2<i32>(gid.xy);
    if (p.x >= dims.x || p.y >= dims.y) {
        return;
    }
    textureStore(out, p, vec4<u32>(0u));
}
"#;

/// Replace `from` (which must occur exactly `n` times) by `to`.
fn subst(text: &str, from: &str, to: &str, n: usize) -> Result<String, String> {
    let found = text.matches(from).count();
    if found != n {
        return Err(format!("8 bpc codes shader: expected {n} × {from:?}, found {found}"));
    }
    Ok(text.replace(from, to))
}

/// The text of top-level function `name` in `src`, with the attribute line right above it
/// (`@compute @workgroup_size(..)`): from `fn name(` to the first `}` at the start of a line.
fn function(src: &str, name: &str) -> Result<String, String> {
    let head = format!("\nfn {name}(");
    let at = src.find(&head).ok_or_else(|| format!("8 bpc codes shader: no fn {name}"))?;
    let start = at.saturating_add(1);
    let attr = src.get(..at).and_then(|before| before.rfind('\n')).and_then(|line| src.get(line.saturating_add(1)..at));
    let begin = match attr {
        Some(a) if a.starts_with("@compute") => at.saturating_sub(a.len()),
        _ => start,
    };
    let body = src.get(start..).ok_or("8 bpc codes shader: bad offset")?;
    let end = body.find("\n}").ok_or_else(|| format!("8 bpc codes shader: fn {name} has no end"))?;
    src.get(begin..start.saturating_add(end).saturating_add(2)).map(str::to_owned).ok_or_else(|| "8 bpc codes shader: bad range".into())
}

/// Entry points of a variant.
fn entries(io: Io) -> Vec<&'static str> {
    let mut e = vec!["warp_blend", "warp_blend_at", "blend_full", "blend_full_at"];
    if io == (Io { src: false, aux: false, out: true }) {
        e.extend(["quantize", "clear"]);
    }
    if io == (Io { src: true, aux: false, out: false }) {
        e.extend(["half", "warp_accum", "warp_accum_at", "decode"]);
    }
    e
}

/// The WGSL of variant `io` (see the module docs).
pub(crate) fn source(io: Io) -> Result<String, String> {
    let mut common = COMMON.to_owned();
    if io.src {
        common = subst(&common, "var src: texture_2d<f32>;", "var src: texture_2d<u32>;", 1)?;
        // The sampling helpers read only `src` here: retyped, each texel decoded.
        common = subst(&common, "(t: texture_2d<f32>,", "(t: texture_2d<u32>,", 6)?;
        common = subst(&common, "return textureLoad(t, vec2<i32>(x, y), 0);", "return dec(textureLoad(t, vec2<i32>(x, y), 0));", 1)?;
        common = subst(
            &common,
            "return textureLoad(t, vec2<i32>(clamp(x, 0, d.x - 1), clamp(y, 0, d.y - 1)), 0);",
            "return dec(textureLoad(t, vec2<i32>(clamp(x, 0, d.x - 1), clamp(y, 0, d.y - 1)), 0));",
            1,
        )?;
    }
    if io.aux {
        common = subst(&common, "var aux: texture_2d<f32>;", "var aux: texture_2d<u32>;", 1)?;
    }
    let out = if io.out { "rgba8uint" } else { "rgba32float" };
    let mut text = format!("{common}\n{DECODE}\n@group(0) @binding(3) var out: texture_storage_2d<{out}, write>;\n");
    if io.out {
        text.push_str(STORE_CODES);
    }
    let names = entries(io);
    let mut fns = vec!["out_dims", "warp_blend_px", "blend_full_px"];
    if !io.out {
        fns.extend(["quantize_px", "store_q"]);
    }
    if names.contains(&"warp_accum") {
        fns.push("warp_accum_px");
    }
    fns.extend(names.iter().copied());
    let mut kernels = String::new();
    if names.contains(&"decode") {
        kernels.push_str(DECODE_ENTRY);
    }
    if names.contains(&"clear") {
        kernels.push_str(CLEAR_ENTRY);
    }
    for f in fns.into_iter().filter(|f| *f != "decode" && *f != "clear") {
        let mut t = function(KERNELS, f)?;
        if f == "quantize" && io.out {
            t = subst(
                &t,
                "textureStore(out, p, quantize_px(textureLoad(src, p, 0), P.f[0].x, P.f[0].y));",
                "store_q(p, textureLoad(src, p, 0), P.f[0].x, P.f[0].y);",
                1,
            )?;
        }
        kernels.push_str(&t);
        kernels.push('\n');
    }
    if io.src {
        kernels = kernels.replace("textureLoad(src, ", "src_load(");
        text.push_str("fn src_load(p: vec2<i32>, l: i32) -> vec4<f32> { return dec(textureLoad(src, p, l)); }\n");
    }
    if io.aux {
        kernels = kernels.replace("textureLoad(aux, ", "aux_load(");
        text.push_str("fn aux_load(p: vec2<i32>, l: i32) -> vec4<f32> { return dec(textureLoad(aux, p, l)); }\n");
    }
    text.push_str(&kernels);
    Ok(text)
}

/// The text of `src`'s section `title`: from its `// ----…---- title` rule to the next rule.
fn section(src: &str, title: &str) -> Result<String, String> {
    const RULE: &str = "// ----------------------------------------------------------------";
    let head = format!("{RULE} {title}\n");
    let at = src.find(&head).ok_or_else(|| format!("8 bpc codes shader: no section {title}"))?;
    let body = src.get(at.saturating_add(head.len())..).ok_or("8 bpc codes shader: bad offset")?;
    let end = body.find(&format!("\n{RULE}")).unwrap_or(body.len());
    body.get(..end).map(str::to_owned).ok_or_else(|| "8 bpc codes shader: bad range".into())
}

const FX_TONE: &str = include_str!("shaders/fx_tone.wgsl");

/// Entry points of the effect variant reading codes by `src` (`aux`: by `aux`).
fn fx_entries(aux: bool) -> &'static [&'static str] {
    if aux { &["shadow_combine"] } else { &["box_h", "box_v", "shadow_make", "pointwise", "fxt_point", "adjust_mix"] }
}

/// The WGSL of the effect kernels reading codes (Gaussian / Fast Box Blur's passes, Drop
/// Shadow, the per-pixel effects, Hue/Saturation's colour ranges, the adjustment layer mix):
/// as [`source`], from the same text, with the effects' sections of `kernels.wgsl` and
/// `fx_tone.wgsl`. `aux`: Drop Shadow's combine reading the layer (`aux`) as codes.
pub(crate) fn fx_source(aux: bool) -> Result<String, String> {
    let mut common = COMMON.to_owned();
    let mut kernels = function(KERNELS, "out_dims")?;
    kernels.push('\n');
    if aux {
        common = subst(&common, "var aux: texture_2d<f32>;", "var aux: texture_2d<u32>;", 1)?;
        kernels.push_str(&function(KERNELS, "shadow_combine")?);
        kernels = subst(&kernels, "textureLoad(aux, ", "aux_load(", 1)?;
    } else {
        common = subst(&common, "var src: texture_2d<f32>;", "var src: texture_2d<u32>;", 1)?;
        common = subst(&common, "(t: texture_2d<f32>,", "(t: texture_2d<u32>,", 6)?;
        common = subst(&common, "return textureLoad(t, vec2<i32>(x, y), 0);", "return dec(textureLoad(t, vec2<i32>(x, y), 0));", 1)?;
        common = subst(
            &common,
            "return textureLoad(t, vec2<i32>(clamp(x, 0, d.x - 1), clamp(y, 0, d.y - 1)), 0);",
            "return dec(textureLoad(t, vec2<i32>(clamp(x, 0, d.x - 1), clamp(y, 0, d.y - 1)), 0));",
            1,
        )?;
        kernels.push_str(&section(KERNELS, "blurs (raster::blur)")?);
        kernels.push('\n');
        kernels.push_str(&function(KERNELS, "shadow_make")?);
        kernels.push('\n');
        kernels.push_str(&section(KERNELS, "per-pixel effects")?);
        kernels.push('\n');
        kernels.push_str(FX_TONE);
        kernels = kernels.replace("textureLoad(src, ", "src_load(");
    }
    let mut text = format!("{common}\n{DECODE}\n@group(0) @binding(3) var out: texture_storage_2d<rgba32float, write>;\n");
    if aux {
        text.push_str("fn aux_load(p: vec2<i32>, l: i32) -> vec4<f32> { return dec(textureLoad(aux, p, l)); }\n");
    } else {
        text.push_str("fn src_load(p: vec2<i32>, l: i32) -> vec4<f32> { return dec(textureLoad(src, p, l)); }\n");
    }
    text.push_str(&kernels);
    Ok(text)
}

/// The WGSL of the codes `display` pass.
pub(crate) fn display_source() -> Result<String, String> {
    let mut d = subst(DISPLAY, "var dsrc: texture_2d<f32>;", "var dsrc: texture_2d<u32>;", 1)?;
    d = function(&d, "display")?;
    let d = subst(&d, "let s = textureLoad(dsrc, p, 0);", "let s = dec(textureLoad(dsrc, p, 0));", 1)?;
    Ok(format!("{DECODE}\n@group(0) @binding(1) var dsrc: texture_2d<u32>;\n@group(0) @binding(3) var dout: texture_storage_2d<rgba8unorm, write>;\n{d}\n"))
}

fn texture_entry(binding: u32, codes: bool) -> wgpu::BindGroupLayoutEntry {
    let sample_type = if codes { wgpu::TextureSampleType::Uint } else { wgpu::TextureSampleType::Float { filterable: false } };
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture { sample_type, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
        count: None,
    }
}

fn storage_texture_entry(binding: u32, format: wgpu::TextureFormat) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture { access: wgpu::StorageTextureAccess::WriteOnly, format, view_dimension: wgpu::TextureViewDimension::D2 },
        count: None,
    }
}

fn buffer_entry(binding: u32, ty: wgpu::BufferBindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

fn compile(device: &wgpu::Device, label: &str, text: String, entries: &[wgpu::BindGroupLayoutEntry], names: &[&'static str]) -> Result<Variant, String> {
    let module = init_resource(device, label, || {
        device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(text.into()) })
    })?;
    let layout = init_resource(device, label, || device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries }))?;
    let pl = init_resource(device, label, || {
        device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some(label), bind_group_layouts: &[Some(&layout)], immediate_size: 0 })
    })?;
    let mut pipes = Vec::with_capacity(names.len());
    for name in names {
        let pipe = init_resource(device, name, || {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: Some(&pl),
                module: &module,
                entry_point: Some(name),
                compilation_options: Default::default(),
                cache: None,
            })
        })?;
        pipes.push((*name, pipe));
    }
    Ok(Variant { layout, pipes })
}

fn build(device: &wgpu::Device, io: Io) -> Result<Variant, String> {
    if io.index() == 0 {
        return Err("the f32 kernels are not a codes variant".into());
    }
    let text = source(io)?;
    let out = if io.out { FORMAT } else { crate::context::FORMAT };
    let layout = [
        buffer_entry(0, wgpu::BufferBindingType::Uniform),
        texture_entry(1, io.src),
        texture_entry(2, io.aux),
        storage_texture_entry(3, out),
        buffer_entry(4, wgpu::BufferBindingType::Storage { read_only: true }),
        buffer_entry(6, wgpu::BufferBindingType::Storage { read_only: true }),
    ];
    compile(device, "8 bpc codes kernels", text, &layout, &entries(io))
}

fn build_fx(device: &wgpu::Device, aux: bool) -> Result<Variant, String> {
    let layout = [
        buffer_entry(0, wgpu::BufferBindingType::Uniform),
        texture_entry(1, !aux),
        texture_entry(2, aux),
        storage_texture_entry(3, crate::context::FORMAT),
        buffer_entry(4, wgpu::BufferBindingType::Storage { read_only: true }),
        buffer_entry(6, wgpu::BufferBindingType::Storage { read_only: true }),
    ];
    compile(device, "8 bpc codes effect kernels", fx_source(aux)?, &layout, fx_entries(aux))
}

fn build_display(device: &wgpu::Device) -> Result<Variant, String> {
    let entries = [
        texture_entry(1, true),
        storage_texture_entry(3, wgpu::TextureFormat::Rgba8Unorm),
        buffer_entry(6, wgpu::BufferBindingType::Storage { read_only: true }),
    ];
    compile(device, "8 bpc codes display", display_source()?, &entries, &["display"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_table_is_the_cpu_quantiser() {
        let t = decode_table();
        for k in 0..256u32 {
            let v = k as f32 / 255.0;
            // The CPU stores `round(v · 255) · (1 / 255)`, the table's value for `k`.
            let cpu = (v * 255.0 + 0.5).floor() * (1.0 / 255.0);
            assert_eq!(t[k as usize].to_bits(), cpu.to_bits(), "code {k}");
        }
    }

    #[test]
    fn every_variant_source_builds_from_the_kernels() {
        for i in 1..8 {
            let io = Io::of(i);
            let s = source(io).unwrap_or_else(|e| panic!("{io:?}: {e}"));
            for name in entries(io) {
                assert!(s.contains(&format!("fn {name}(")), "{io:?} lacks {name}");
            }
            // Only the loader itself reads the codes texture.
            if io.src {
                assert_eq!(s.matches("textureLoad(src, ").count(), 1, "{io:?} reads src undecoded");
            }
            if io.aux {
                assert_eq!(s.matches("textureLoad(aux, ").count(), 1, "{io:?} reads aux undecoded");
            }
        }
        display_source().unwrap();
        for aux in [false, true] {
            let s = fx_source(aux).unwrap_or_else(|e| panic!("fx {aux}: {e}"));
            for name in fx_entries(aux) {
                assert!(s.contains(&format!("fn {name}(")), "fx {aux} lacks {name}");
            }
            let t = if aux { "aux" } else { "src" };
            assert_eq!(s.matches(&format!("textureLoad({t}, ")).count(), 1, "fx {aux} reads {t} undecoded");
        }
    }
}
