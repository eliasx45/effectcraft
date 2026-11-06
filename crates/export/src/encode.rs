//! Movie export: FilmCraft H.264, HEVC (`effectcraft-hevcenc`) and AV1 (`effectcraft-av1enc`)
//! → MP4 (+ AAC), ProRes and uncompressed `raw ` → MOV (+ PCM), muxed by
//! FilmCraft's ISO BMFF / QuickTime writer.

use std::io::Write;

use effectcraft_project::Comp;
use effectcraft_project::render_queue::{AudioFormat, Channels, OutputFormat, ProResProfile};
use effectcraft_time::{FrameRate, Tick};
use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, PcmConfig, SampleEntry, TrackConfig, WriteSample, WriterOptions};
use rayon::prelude::*;

use crate::pipeline::Batches;
use crate::{Cx, ExportError, Report, Result, State, batch_size, wants_audio};

fn enc(e: impl std::fmt::Display) -> ExportError {
    ExportError::Encode(e.to_string())
}
fn mux_err(e: impl std::fmt::Display) -> ExportError {
    ExportError::Io(e.to_string())
}

pub(crate) struct Packet {
    pub(crate) data: Vec<u8>,
    pub(crate) key: bool,
    /// pts − dts in the track timescale.
    pub(crate) cto: i32,
}

pub(crate) trait VideoEncoder {
    fn sample_entry(&self) -> SampleEntry;
    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>>;
    /// Encode a batch of frames (the first at `first`), in order. Encoders that can encode
    /// frames independently do them in parallel.
    fn encode_batch(&mut self, frames: &[Vec<u8>], first: u64) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        for (k, px) in frames.iter().enumerate() {
            out.extend(self.encode(px, first + k as u64)?);
        }
        Ok(out)
    }
    fn flush(&mut self) -> Result<Vec<Packet>>;
    /// Edit-list media start (B-frame delay) in the track timescale.
    fn media_start(&self) -> Option<i64> {
        None
    }
    /// A hardware encoder (the movie is encoded in software instead when its first frames fail).
    fn hardware(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------- H.264

/// The H.264 key-frame interval: 2 s of frames.
pub(crate) fn h264_keyint(rate: FrameRate) -> u32 {
    (rate.as_f64() * 2.0).round().max(1.0) as u32
}

/// FilmCraft H.264 High for MP4 samples: VBR at `kbps` (peak 1.5×), a key frame every 2 s.
pub(crate) fn h264_config(w: u32, h: u32, rate: FrameRate, kbps: u32) -> filmcraft_h264enc::EncoderConfig {
    let mut cfg = filmcraft_h264enc::EncoderConfig::new(w, h, rate.num as u32, rate.den as u32);
    cfg.format = filmcraft_h264enc::PacketFormat::LengthPrefixed;
    cfg.aud = false;
    cfg.keyint = h264_keyint(rate);
    let kbps = kbps.max(100);
    cfg.rate = filmcraft_h264enc::RateControl::Vbr { target_kbps: kbps, max_kbps: kbps * 3 / 2 };
    cfg
}

/// The `avc1` sample entry of an `avcC` record, or of the parameter sets when it doesn't parse.
pub(crate) fn avc_entry(avcc: &[u8], ps: Option<(Vec<u8>, Vec<u8>)>, w: u32, h: u32) -> SampleEntry {
    let cfg = filmcraft_isobmff::AvcConfig::parse(avcc).unwrap_or_else(|_| {
        let (sps, pps) = ps.unwrap_or_default();
        filmcraft_isobmff::AvcConfig::new(vec![sps], vec![pps], 4)
    });
    SampleEntry::avc(cfg, w as u16, h as u16)
}

/// The H.264 encoder of a movie of `total` frames: the hardware encoder when asked for
/// ([`crate::speed::hardware_h264`]) and one opens, else [`software_h264`].
fn h264_encoder(w: u32, h: u32, rate: FrameRate, kbps: u32, total: u64) -> Result<Box<dyn VideoEncoder>> {
    if crate::speed::hardware_h264() {
        match HwH264::open(w, h, rate, kbps) {
            Ok(e) => {
                log::info!(target: "effectcraft_export", "H.264: hardware encoder");
                return Ok(Box::new(e));
            }
            Err(e) => log::warn!(target: "effectcraft_export", "H.264: no hardware encoder ({e}), encoding in software"),
        }
    }
    software_h264(w, h, rate, kbps, total)
}

/// FilmCraft's encoder: parallel keyframe groups ([`crate::h264_groups`]) when the movie has more
/// than one group to run at once and [`crate::speed::parallel_h264`], else one encoder.
fn software_h264(w: u32, h: u32, rate: FrameRate, kbps: u32, total: u64) -> Result<Box<dyn VideoEncoder>> {
    let cfg = h264_config(w, h, rate, kbps);
    if cfg!(not(target_arch = "wasm32")) && crate::speed::parallel_h264() {
        use crate::h264_groups::{GROUPS_ENV, GroupedH264, QUEUE_BUDGET, THREADS_ENV, plan, plan_with_env};
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let p = plan(w, h, u64::from(cfg.keyint), total, cores, QUEUE_BUDGET);
        let p = plan_with_env(p, std::env::var(GROUPS_ENV).ok().as_deref(), std::env::var(THREADS_ENV).ok().as_deref());
        if p.lanes > 1 {
            let threads = if p.threads == 0 { "all cores".to_string() } else { format!("{} threads", p.threads) };
            log::info!(target: "effectcraft_export", "H.264: {} keyframe groups at once, {threads} each", p.lanes);
            return Ok(Box::new(GroupedH264::new(cfg, rate.den, p)?));
        }
    }
    Ok(Box::new(H264::with_config(cfg, rate)?))
}

pub(crate) struct H264 {
    enc: filmcraft_h264enc::Encoder,
    w: u32,
    h: u32,
    rate: FrameRate,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl H264 {
    pub(crate) fn with_config(cfg: filmcraft_h264enc::EncoderConfig, rate: FrameRate) -> Result<H264> {
        let (w, h) = (cfg.width, cfg.height);
        let enc = filmcraft_h264enc::Encoder::new(cfg).map_err(enc)?;
        Ok(H264 { enc, w, h, rate, y: vec![], u: vec![], v: vec![] })
    }
    fn packets(&self, ps: Vec<filmcraft_h264enc::Packet>) -> Vec<Packet> {
        ps.into_iter().map(|p| Packet { data: p.data, key: p.keyframe, cto: (p.pts - p.dts) as i32 }).collect()
    }
}

impl VideoEncoder for H264 {
    fn sample_entry(&self) -> SampleEntry {
        avc_entry(&self.enc.avcc(), Some(self.enc.sps_pps()), self.w, self.h)
    }
    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        rgba_to_yuv420(rgba, self.w as usize, self.h as usize, &mut self.y, &mut self.u, &mut self.v);
        let cw = (self.w as usize).div_ceil(2);
        let frame = filmcraft_h264enc::YuvFrame { y: &self.y, u: &self.u, v: &self.v, y_stride: self.w as usize, uv_stride: cw };
        let ps = self.enc.try_encode(&frame, index as i64 * self.rate.den).map_err(enc)?;
        Ok(self.packets(ps))
    }
    fn flush(&mut self) -> Result<Vec<Packet>> {
        let ps = self.enc.flush();
        Ok(self.packets(ps))
    }
    fn media_start(&self) -> Option<i64> {
        (self.enc.delay() > 0).then_some(self.rate.den)
    }
}

/// H.264 on the platform's hardware encoder (`effectcraft_media::h264_hw`): no B-frames, so
/// samples come out in presentation order with no composition offsets and no edit.
struct HwH264 {
    session: Box<dyn effectcraft_media::h264_hw::H264Session>,
    cfg: effectcraft_media::h264_hw::H264Config,
    /// The stream's SPS / PPS (from its first sample).
    ps: Option<(Vec<u8>, Vec<u8>)>,
    /// Pictures handed in and samples handed out.
    sent: u64,
    got: u64,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl HwH264 {
    fn open(w: u32, h: u32, rate: FrameRate, kbps: u32) -> std::result::Result<HwH264, String> {
        let cfg = effectcraft_media::h264_hw::H264Config {
            width: w,
            height: h,
            fps_num: u32::try_from(rate.num).map_err(|_| "frame rate")?,
            fps_den: u32::try_from(rate.den).map_err(|_| "frame rate")?,
            kbps: kbps.max(100),
            keyint: h264_keyint(rate),
        };
        let session = effectcraft_media::h264_hw::open(&cfg)?;
        Ok(HwH264 { session, cfg, ps: None, sent: 0, got: 0, y: vec![], u: vec![], v: vec![] })
    }

    fn take(&mut self, samples: Vec<effectcraft_media::h264_hw::H264Sample>) -> Result<Vec<Packet>> {
        if self.ps.is_none() {
            self.ps = self.session.parameter_sets();
        }
        let mut out = Vec::with_capacity(samples.len());
        for s in samples {
            if s.index != self.got {
                return Err(ExportError::Encode(format!("the hardware H.264 encoder returned picture {} where {} was due", s.index, self.got)));
            }
            self.got += 1;
            out.push(Packet { data: s.data, key: s.key, cto: 0 });
        }
        Ok(out)
    }

    /// After the session failed on picture `index`: a new session (it starts with an IDR) codes
    /// it again, and the stream goes on if its parameter sets are the same.
    fn reopen(&mut self, index: u64, why: String) -> Result<Vec<Packet>> {
        log::warn!(target: "effectcraft_export", "hardware H.264 encoder failed at frame {index} ({why}): a new session");
        let mut s = effectcraft_media::h264_hw::open(&self.cfg)
            .map_err(|e| ExportError::Encode(format!("hardware H.264 encoder failed ({why}) and could not restart: {e}")))?;
        let mut samples = s.encode(&self.y, &self.u, &self.v, index).map_err(enc)?;
        samples.extend(s.complete().map_err(enc)?);
        if s.parameter_sets() != self.ps {
            return Err(ExportError::Encode(format!("hardware H.264 encoder failed at frame {index} ({why}); export again without hardware encoding")));
        }
        self.session = s;
        self.take(samples)
    }
}

impl VideoEncoder for HwH264 {
    fn sample_entry(&self) -> SampleEntry {
        let (sps, pps) = self.ps.clone().unwrap_or_default();
        SampleEntry::avc(filmcraft_isobmff::AvcConfig::new(vec![sps], vec![pps], 4), self.cfg.width as u16, self.cfg.height as u16)
    }
    fn encode(&mut self, rgba: &[u8], index: u64) -> Result<Vec<Packet>> {
        rgba_to_yuv420(rgba, self.cfg.width as usize, self.cfg.height as usize, &mut self.y, &mut self.u, &mut self.v);
        self.sent += 1;
        match self.session.encode(&self.y, &self.u, &self.v, index) {
            Ok(samples) => self.take(samples),
            Err(e) if self.ps.is_some() => self.reopen(index, e),
            Err(e) => Err(ExportError::Encode(e)),
        }
    }
    fn encode_batch(&mut self, frames: &[Vec<u8>], first: u64) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        for (k, px) in frames.iter().enumerate() {
            out.extend(self.encode(px, first + k as u64)?);
        }
        if self.ps.is_none() && !frames.is_empty() {
            // the sample entry is made after the first batch: its parameter sets are needed now
            let done = self.session.complete().map_err(enc)?;
            out.extend(self.take(done)?);
            if self.ps.is_none() {
                return Err(ExportError::Encode("the hardware H.264 encoder gave no parameter sets".into()));
            }
        }
        Ok(out)
    }
    fn flush(&mut self) -> Result<Vec<Packet>> {
        let done = self.session.complete().map_err(enc)?;
        let out = self.take(done)?;
        if self.got != self.sent {
            return Err(ExportError::Encode(format!("the hardware H.264 encoder returned {} of {} frames", self.got, self.sent)));
        }
        Ok(out)
    }
    fn hardware(&self) -> bool {
        true
    }
}

/// BT.709 limited-range 8-bit 4:2:0 from RGBA8 (2×2 chroma average; alpha ignored).
pub(crate) fn rgba_to_yuv420(rgba: &[u8], w: usize, h: usize, y: &mut Vec<u8>, u: &mut Vec<u8>, v: &mut Vec<u8>) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    y.resize(w * h, 0);
    u.resize(cw * ch, 0);
    v.resize(cw * ch, 0);
    y.par_chunks_mut(w * 2).zip(u.par_chunks_mut(cw).zip(v.par_chunks_mut(cw))).enumerate().for_each(|(cy, (yr, (ur, vr)))| {
        let rows = yr.len() / w;
        let mut acc = vec![(0f32, 0f32, 0f32); cw];
        for dy in 0..rows {
            let src = &rgba[(cy * 2 + dy) * w * 4..][..w * 4];
            for x in 0..w {
                let (r, g, b) = (src[x * 4] as f32 / 255.0, src[x * 4 + 1] as f32 / 255.0, src[x * 4 + 2] as f32 / 255.0);
                let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                yr[dy * w + x] = (16.0 + 219.0 * l).round().clamp(1.0, 254.0) as u8;
                let a = &mut acc[x / 2];
                a.0 += (b - l) / 1.8556;
                a.1 += (r - l) / 1.5748;
                a.2 += 1.0;
            }
        }
        for (cx, a) in acc.iter().enumerate() {
            ur[cx] = (128.0 + 224.0 * a.0 / a.2).round().clamp(1.0, 254.0) as u8;
            vr[cx] = (128.0 + 224.0 * a.1 / a.2).round().clamp(1.0, 254.0) as u8;
        }
    });
}

// ---------------------------------------------------------------- ProRes

/// Frames a ProRes export encodes at once (one hardware session each: `effectcraft_platform`'s
/// pool holds 4; a 4K 4444 frame's planes are ~66 MB).
const PRORES_PARALLEL: usize = 4;

/// ProRes through `effectcraft_media::prores_enc`: VideoToolbox's hardware encoder on a Mac with
/// ProRes engines, FilmCraft's software encoder for any frame it doesn't take or fails on.
struct ProRes {
    enc: effectcraft_media::prores_enc::ProResEncoder,
    profile: filmcraft_prores::Profile,
    w: u32,
    h: u32,
    alpha: bool,
}

fn prores_profile(p: ProResProfile) -> filmcraft_prores::Profile {
    use filmcraft_prores::Profile;
    match p {
        ProResProfile::Proxy => Profile::Proxy,
        ProResProfile::Lt => Profile::Lt,
        ProResProfile::Standard => Profile::Standard,
        ProResProfile::Hq => Profile::Hq,
        ProResProfile::P4444 => Profile::P4444,
        ProResProfile::P4444Xq => Profile::P4444Xq,
    }
}

impl ProRes {
    fn new(w: u32, h: u32, profile: ProResProfile, channels: Channels) -> ProRes {
        // Alpha needs a 4444 flavour.
        let profile = if channels == Channels::Rgba && !profile.is_4444() { ProResProfile::P4444 } else { profile };
        let profile = prores_profile(profile);
        let alpha = channels == Channels::Rgba;
        let mut cfg = filmcraft_prores::EncoderConfig::new(profile, w, h);
        cfg.encode_alpha = alpha;
        ProRes { enc: effectcraft_media::prores_enc::ProResEncoder::new(cfg), profile, w, h, alpha }
    }
    fn is_444(&self) -> bool {
        matches!(self.profile, filmcraft_prores::Profile::P4444 | filmcraft_prores::Profile::P4444Xq)
    }
    /// The encoder's input for one RGBA8 frame: 10-bit BT.709 Y'CbCr (+ alpha).
    fn frame(&self, rgba: &[u8]) -> filmcraft_prores::Frame {
        let chroma = if self.is_444() { filmcraft_prores::ChromaFormat::Yuv444 } else { filmcraft_prores::ChromaFormat::Yuv422 };
        let mut fr = filmcraft_prores::Frame::new(self.w, self.h, chroma, 10, self.alpha);
        rgba_to_yuv_10(rgba, self.w as usize, self.h as usize, self.is_444(), &mut fr.y, &mut fr.cb, &mut fr.cr);
        if let Some(a) = fr.alpha.as_mut() {
            for (d, s) in a.iter_mut().zip(rgba.as_chunks::<4>().0.iter()) {
                *d = ((s[3] as u32 * 1023 + 127) / 255) as u16;
            }
        }
        fr
    }
}

impl VideoEncoder for ProRes {
    fn sample_entry(&self) -> SampleEntry {
        SampleEntry::prores(FourCc(self.profile.fourcc()), self.w as u16, self.h as u16)
    }
    fn encode(&mut self, rgba: &[u8], _index: u64) -> Result<Vec<Packet>> {
        let fr = self.frame(rgba);
        let data = self.enc.encode(&fr).map_err(enc)?;
        Ok(vec![Packet { data, key: true, cto: 0 }])
    }
    fn encode_batch(&mut self, frames: &[Vec<u8>], _first: u64) -> Result<Vec<Packet>> {
        let mut out = Vec::with_capacity(frames.len());
        for chunk in frames.chunks(PRORES_PARALLEL) {
            let this = &*self;
            let done: Vec<Result<Vec<u8>>> = chunk
                .par_iter()
                .map(|px| {
                    let fr = this.frame(px);
                    this.enc.clone().encode(&fr).map_err(enc)
                })
                .collect();
            for data in done {
                out.push(Packet { data: data?, key: true, cto: 0 });
            }
        }
        Ok(out)
    }
    fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(vec![])
    }
}

/// BT.709 limited-range 10-bit 4:2:2 (horizontal chroma average) or 4:4:4 from RGBA8.
fn rgba_to_yuv_10(rgba: &[u8], w: usize, h: usize, full_chroma: bool, y: &mut [u16], cb: &mut [u16], cr: &mut [u16]) {
    let cw = if full_chroma { w } else { w.div_ceil(2) };
    debug_assert!(y.len() >= w * h && rgba.len() >= w * h * 4);
    y.par_chunks_mut(w).zip(cb.par_chunks_mut(cw).zip(cr.par_chunks_mut(cw))).enumerate().for_each(|(row, (yr, (cbr, crr)))| {
        let src = &rgba[row * w * 4..(row + 1) * w * 4];
        let mut us = vec![0f32; w];
        let mut vs = vec![0f32; w];
        for x in 0..w {
            let (r, g, b) = (src[x * 4] as f32 / 255.0, src[x * 4 + 1] as f32 / 255.0, src[x * 4 + 2] as f32 / 255.0);
            let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            yr[x] = (64.0 + 876.0 * l).round().clamp(4.0, 1019.0) as u16;
            us[x] = (b - l) / 1.8556;
            vs[x] = (r - l) / 1.5748;
        }
        for cx in 0..cw {
            let (u, v) = if full_chroma {
                (us[cx], vs[cx])
            } else {
                let (a, b2) = (cx * 2, (cx * 2 + 1).min(w - 1));
                ((us[a] + us[b2]) * 0.5, (vs[a] + vs[b2]) * 0.5)
            };
            cbr[cx] = (512.0 + 896.0 * u).round().clamp(4.0, 1019.0) as u16;
            crr[cx] = (512.0 + 896.0 * v).round().clamp(4.0, 1019.0) as u16;
        }
    });
}

// ---------------------------------------------------------------- audio

enum Audio {
    Aac(Box<filmcraft_aac::Encoder>),
    Pcm,
}

fn deinterleave(buf: &[f32], channels: usize) -> Vec<Vec<f32>> {
    (0..channels).map(|c| buf.chunks_exact(channels).map(|p| p[c]).collect()).collect()
}

/// The comp's audio over `n` samples from `start`, interleaved with the module's channel count
/// (mono = the average of left and right).
pub(crate) fn mix(cx: &Cx, start: effectcraft_time::Tick, n: usize, sr: u32) -> Vec<f32> {
    let st = effectcraft_render::audio::mix_comp(&cx.project, cx.source(), cx.expr, cx.comp, start, n, sr);
    if cx.output.audio_channels == 1 { st.as_chunks::<2>().0.iter().map(|p| (p[0] + p[1]) * 0.5).collect() } else { st }
}

/// PCM bytes of samples in `fmt` (little or big endian).
pub(crate) fn pcm_bytes(buf: &[f32], fmt: AudioFormat, big_endian: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len() * 4);
    for &s in buf {
        let s = s.clamp(-1.0, 1.0);
        match fmt {
            AudioFormat::S16 => {
                let v = (s * 32767.0).round() as i16;
                out.extend_from_slice(&if big_endian { v.to_be_bytes() } else { v.to_le_bytes() });
            }
            AudioFormat::S24 => {
                let v = (s * 8_388_607.0).round() as i32;
                let b = v.to_le_bytes();
                if big_endian {
                    out.extend_from_slice(&[b[2], b[1], b[0]]);
                } else {
                    out.extend_from_slice(&b[..3]);
                }
            }
            AudioFormat::F32 => out.extend_from_slice(&if big_endian { s.to_be_bytes() } else { s.to_le_bytes() }),
        }
    }
    out
}

// ---------------------------------------------------------------- the movie

type Writer<'f, 's> = Mp4Writer<&'f mut crate::out::Out<'s>>;

pub(crate) async fn movie(job: &Cx<'_>, comp: &Comp, w: u32, h: u32, st: &mut State<'_>) -> Result<Report> {
    let rate = job.settings.rate(comp);
    let fmt = job.output.format;
    let channels = match job.output.channels {
        Channels::Rgba if !matches!(fmt, OutputFormat::ProRes | OutputFormat::Uncompressed) => Channels::Rgb,
        c => c,
    };
    let mut venc: Box<dyn VideoEncoder> = match fmt {
        OutputFormat::H264 => h264_encoder(w, h, rate, job.output.bitrate_kbps, st.total)?,
        OutputFormat::Hevc => Box::new(crate::hevc_av1::Hevc::new(w, h, rate, job.output)?),
        OutputFormat::Av1 => Box::new(crate::hevc_av1::Av1::new(w, h, rate, job.output)?),
        OutputFormat::Uncompressed => Box::new(crate::uncompressed::Raw::new(w, h, channels == Channels::Rgba)?),
        _ => Box::new(ProRes::new(w, h, job.output.prores_profile, if channels == Channels::Alpha { Channels::Rgb } else { channels })),
    };
    let brand = if matches!(fmt, OutputFormat::ProRes | OutputFormat::Uncompressed) { Brand::Mov } else { Brand::Mp4 };
    let batch = batch_size(job, comp);
    let render = |_: u64, img: effectcraft_raster::Image| -> Vec<u8> { job.pixels(&img, comp, channels, w, h) };

    // The next batch renders while this one encodes and muxes.
    let total = st.total;
    job.batches(comp, total, batch, render, async |batches: &mut Batches<'_, '_, Vec<u8>>| -> Result<Report> {
        // Encode the first batch before creating tracks: encoders finalise their config on frame 1.
        let first_end = batch.min(total);
        let first: Vec<Vec<u8>> = batches.next().await?.map(|(_, _, f)| f).unwrap_or_default();
        let pending = match venc.encode_batch(&first, 0) {
            Ok(p) => p,
            // nothing is written yet: the whole movie encodes in software instead
            Err(e) if venc.hardware() => {
                log::warn!(target: "effectcraft_export", "hardware encoder failed on the first frames ({e}): encoding in software");
                venc = software_h264(w, h, rate, job.output.bitrate_kbps, total)?;
                venc.encode_batch(&first, 0)?
            }
            Err(e) => return Err(e),
        };
        drop(first);
        st.advance(first_end)?;

        let path = job.place(job.path, 1);
        let mut file = job.create(&path)?;
        let opts = WriterOptions::new(brand);
        let movie_ts = opts.movie_timescale.max(1) as i128;
        let mut mux: Writer = Mp4Writer::new(&mut file, opts).map_err(mux_err)?;
        let mut vcfg = TrackConfig::new(venc.sample_entry(), rate.num as u32);
        if let Some(start) = venc.media_start() {
            // An explicit edit: the convenience `media_start` edit would subtract the B-frame delay
            // from the media duration and drop the last frame.
            let frames_ts = st.total as i128 * rate.den as i128;
            let seg = (frames_ts * movie_ts + rate.num as i128 / 2) / rate.num as i128;
            vcfg.edits = vec![filmcraft_isobmff::Edit { segment_duration: seg as u64, media_time: start, media_rate: 0x10000 }];
        }
        let vt = mux.add_track(vcfg).map_err(mux_err)?;
        let sr = job.output.audio_sample_rate.clamp(8_000, 192_000);
        let with_audio = wants_audio(job);
        let chans = if job.output.audio_channels == 1 { 1u32 } else { 2 };
        let afmt = job.output.audio_format;
        let mut audio = None;
        if with_audio {
            let (a, entry, start) = if brand == Brand::Mp4 {
                let e = filmcraft_aac::Encoder::new(filmcraft_aac::EncoderConfig::cbr(sr, chans as usize, 160_000 * chans)).map_err(enc)?;
                let entry = SampleEntry::aac(e.audio_specific_config(), chans, sr);
                let priming = e.priming_samples() as i64;
                (Audio::Aac(Box::new(e)), entry, Some(priming))
            } else {
                let (bits, float) = match afmt {
                    AudioFormat::S16 => (16, false),
                    AudioFormat::S24 => (24, false),
                    AudioFormat::F32 => (32, true),
                };
                let pcm = PcmConfig { bits, float, big_endian: false, signed: true, channels: chans, sample_rate: sr as f64 };
                (Audio::Pcm, SampleEntry::pcm(pcm), None)
            };
            let mut c = TrackConfig::new(entry, sr);
            c.media_start = start;
            let at = mux.add_track(c).map_err(mux_err)?;
            audio = Some((a, at));
        }
        let (span_start, span_end) = job.settings.span(comp);
        let mut cursor = span_start.to_units_floor(sr as i64);

        // Video samples written so far: audio is interleaved up to the end of their frames (an
        // encoder may hand frames out well after they went in: B-frames, keyframe groups).
        let mut muxed = 0u64;
        let mut write_video = |mux: &mut Writer, ps: Vec<Packet>| -> Result<u64> {
            for p in ps {
                mux.write_sample(vt, WriteSample { data: &p.data, duration: rate.den as u32, composition_offset: p.cto, is_sync: p.key }).map_err(mux_err)?;
                muxed += 1;
            }
            Ok(muxed)
        };
        let mut write_audio_until = |mux: &mut Writer, until: Tick, cursor: &mut i64| -> Result<()> {
            let Some((a, at)) = audio.as_mut() else { return Ok(()) };
            let end = until.to_units_floor(sr as i64);
            if end <= *cursor {
                return Ok(());
            }
            let n = (end - *cursor) as usize;
            let start = Tick::from_units(*cursor, i64::from(sr));
            let start = if start.to_units_floor(i64::from(sr)) < *cursor { Tick(start.0.saturating_add(1)) } else { start };
            let buf = mix(job, start, n, sr);
            *cursor = end;
            match a {
                Audio::Aac(e) => {
                    let planar = deinterleave(&buf, chans as usize);
                    let refs: Vec<&[f32]> = planar.iter().map(Vec::as_slice).collect();
                    for au in e.encode(&refs) {
                        mux.write_sample(*at, WriteSample { data: &au, duration: 1024, composition_offset: 0, is_sync: true }).map_err(mux_err)?;
                    }
                }
                Audio::Pcm => {
                    let pcm = pcm_bytes(&buf, afmt, false);
                    mux.write_sample(*at, WriteSample { data: &pcm, duration: n as u32, composition_offset: 0, is_sync: true }).map_err(mux_err)?;
                }
            }
            Ok(())
        };
        let frame_end_time = |k: u64| -> Tick { if k >= total { span_end } else { job.settings.frame_time(comp, k) } };
        let n = write_video(&mut mux, pending)?;
        write_audio_until(&mut mux, frame_end_time(n.min(first_end)), &mut cursor)?;
        while let Some((i, end, frames)) = batches.next().await? {
            let ps = venc.encode_batch(&frames, i)?;
            let n = write_video(&mut mux, ps)?;
            write_audio_until(&mut mux, frame_end_time(n.min(end)), &mut cursor)?;
            st.advance(end - i)?;
        }
        write_video(&mut mux, venc.flush()?)?;
        write_audio_until(&mut mux, span_end, &mut cursor)?;
        if let Some((Audio::Aac(e), at)) = audio.as_mut() {
            for au in e.flush() {
                mux.write_sample(*at, WriteSample { data: &au, duration: 1024, composition_offset: 0, is_sync: true }).map_err(mux_err)?;
            }
        }
        let out = mux.finish().map_err(mux_err)?;
        out.flush().map_err(mux_err)?;
        let bytes = file.finish()?;
        Ok(Report { path, frames: 0, width: w, height: h, seconds: 0.0, bytes, audio: with_audio, log: None, overflow: vec![] })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuv_of_primaries() {
        let rgba = [255u8, 255, 255, 255, 0, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255];
        let (mut y, mut u, mut v) = (vec![], vec![], vec![]);
        rgba_to_yuv420(&rgba, 2, 2, &mut y, &mut u, &mut v);
        assert_eq!(y[0], 235);
        assert_eq!(y[1], 16);
        assert_eq!((u.len(), v.len()), (1, 1));
        let (mut y10, mut cb, mut cr) = (vec![0u16; 4], vec![0u16; 4], vec![0u16; 4]);
        rgba_to_yuv_10(&rgba, 2, 2, true, &mut y10, &mut cb, &mut cr);
        assert_eq!(y10[0], 940);
        assert_eq!(y10[1], 64);
        assert_eq!(cb[0], 512);
        assert!(cr[2] > 900, "red has high Cr: {}", cr[2]);
    }
}
