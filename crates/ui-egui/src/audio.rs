//! Real-time audio preview.
//!
//! The frontend owns no audio API: the native app supplies an [`AudioDevice`] (cpal) through
//! [`crate::Hooks::audio_device`], the web app one on Web Audio. During preview playback an
//! [`AudioPlayback`] runs a feeder thread (wasm32: [`AudioPlayback::pump`] every UI frame) that
//! mixes the composition ahead of the device with the same mixdown export uses
//! (`effectcraft_render::audio::mix_comp`: Audio switch, solo, Audio Levels, audio effects) and
//! pushes it into an [`AudioFeed`]; the device callback pulls from the feed.
//!
//! **A/V sync:** while audio plays, the audio clock drives playback. The clock is the number of
//! real sample frames audible at the device (native playback timestamps interpolate within
//! submitted buffers; other hosts use consumed minus latency), mapped through the work-area
//! loop ([`clock_sample`]). Underrun silence does not advance it; the viewer
//! shows the comp frame at that sample ([`frame_of_sample`]), dropping video frames rather
//! than drifting.
//!
//! The feed also tracks per-channel peaks for the Audio panel's VU meters ([`Meter`]).
//!
//! **Play Faster** (×2, ×4, ×8): the feeder mixes that many times more of the comp per block and
//! compresses it in time at the same pitch ([`Stretch`]); the clock counts each device frame as
//! that many comp samples.
//!
//! **Scrubbing:** Ctrl/Cmd-dragging the current-time indicator plays a short snippet of the mix
//! at each new frame ([`AudioScrub`], `playback.scrubAudio`), as in After Effects.
//!
//! **Hover-to-hear:** a Motion Library Sound card plays its file while the pointer is over it
//! ([`SoundPreview`], on an output of its own, silent while the Timeline plays).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use effectcraft_engine::project::ItemId;
use effectcraft_engine::render::audio::AudioStream;
use effectcraft_engine::time::{FrameRate, Tick};

use crate::frames::RenderSource;

pub use crate::audio_stretch::Stretch;

/// An audio output (implemented by the native app with cpal).
pub trait AudioDevice: Send {
    /// Device sample rate in Hz.
    fn sample_rate(&self) -> u32;
    /// Start pulling interleaved stereo frames from `feed` (see [`AudioFeed::pull`]).
    fn start(&mut self, feed: Arc<AudioFeed>) -> Result<(), String>;
    fn stop(&mut self);
    /// Output latency in sample frames (device buffering), for A/V sync.
    fn latency_frames(&self) -> u64 {
        0
    }
    /// Real sample frames audible now, before composition looping. Native devices can use
    /// playback timestamps to interpolate within submitted buffers instead of stepping once
    /// per callback. `None` keeps the consumed-minus-latency clock for other hosts.
    fn audible_frames(&self) -> Option<u64> {
        None
    }
    /// Called every UI frame while playing: devices without an audio thread of their own (Web
    /// Audio) move queued samples to the output here.
    fn pump(&mut self) {}
    /// Seconds from [`Self::start`] until the first non-silent sample was audible (the device's
    /// playback timestamp), once it was: the play-to-sound latency `ui.inspect` and the log
    /// report. `None` before that, and for devices that don't measure it.
    fn first_sound(&self) -> Option<f64> {
        None
    }
}

/// Opens the default output device (`None` when there is none).
pub type AudioDeviceFactory = Box<dyn Fn(&AudioOutput) -> Option<Box<dyn AudioDevice>>>;

/// Which output to open (Settings ▸ Audio): device name (empty = the system default) and the
/// 1-based device channels for left and right.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioOutput {
    pub device: String,
    pub left: u32,
    pub right: u32,
    /// Preview Sample Rate: devices that support it are opened at this rate.
    pub rate: u32,
}

impl AudioOutput {
    pub fn from_prefs(p: &effectcraft_engine::prefs::Prefs) -> AudioOutput {
        AudioOutput { device: p.audio.output_device.clone(), left: p.audio.output_left, right: p.audio.output_right, rate: p.audio.preview_sample_rate }
    }
}

/// Lock-protected sample queue between the feeder thread and the device callback.
#[derive(Default)]
pub struct AudioFeed {
    queue: Mutex<VecDeque<f32>>,
    /// Sample frames handed to the device (real audio only: underruns do not count).
    consumed: AtomicU64,
    /// Peak |sample| per channel since the last [`AudioFeed::take_peaks`] (f32 bits).
    peak: [AtomicU32; 2],
    /// The feeder reached the end of a non-looping span.
    finished: AtomicBool,
    /// Bumped (under the queue lock) when playback holds or moves: blocks mixed for the old
    /// position are not queued ([`AudioFeed::push_at`]).
    epoch: AtomicU64,
    /// Playback is held: the feeder mixes nothing and the device plays silence.
    held: AtomicBool,
}

impl AudioFeed {
    /// Fill `out` (interleaved stereo) from the queue; silence where the queue runs dry.
    pub fn pull(&self, out: &mut [f32]) {
        let mut real = 0usize;
        let mut pk = [0.0f32; 2];
        if let Ok(mut q) = self.queue.lock() {
            let n = q.len().min(out.len()) & !1;
            for (i, (o, s)) in out.iter_mut().zip(q.drain(..n)).enumerate() {
                *o = s;
                pk[i & 1] = pk[i & 1].max(s.abs());
            }
            real = n;
            // Publish consumption before an empty queue becomes visible to finished(): the
            // last hardware buffer must not be mistaken for already drained audio.
            self.consumed.fetch_add((real / 2) as u64, Ordering::Relaxed);
        }
        out[real..].iter_mut().for_each(|o| *o = 0.0);
        for (c, v) in pk.into_iter().enumerate() {
            // Rust 1.99 renamed `fetch_update` to `try_update`; keep the old name while the
            // workspace supports Rust 1.95 (`rust-version`).
            #[allow(deprecated)]
            let _ = self.peak[c].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| (v > f32::from_bits(old)).then_some(v.to_bits()));
        }
    }

    /// Drop what is still queued (scrubbing replaces the snippet still playing).
    pub fn clear(&self) {
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
        }
    }

    /// Keep only the next `frames` queued, fading them to silence, and start a new epoch: what
    /// was mixed for the old sound is not queued after it ([`AudioFeed::push_at`]).
    pub fn fade_out(&self, frames: usize) {
        if let Ok(mut q) = self.queue.lock() {
            let keep = frames.min(q.len() / 2);
            q.truncate(keep * 2);
            for i in 0..keep {
                let g = 1.0 - (i + 1) as f32 / keep as f32;
                for c in 0..2 {
                    if let Some(v) = q.get_mut(2 * i + c) {
                        *v *= g;
                    }
                }
            }
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn push(&self, samples: &[f32]) {
        if let Ok(mut q) = self.queue.lock() {
            q.extend(samples.iter().copied());
        }
    }

    /// Queue `samples` mixed at feed epoch `epoch`, unless playback held or moved since.
    fn push_at(&self, epoch: u64, samples: &[f32]) -> bool {
        let Ok(mut q) = self.queue.lock() else { return false };
        if self.epoch.load(Ordering::Acquire) != epoch {
            return false;
        }
        q.extend(samples.iter().copied());
        true
    }

    /// Drop what is queued and start a new epoch (held or not).
    fn restart(&self, held: bool) {
        if let Ok(mut q) = self.queue.lock() {
            q.clear();
            self.held.store(held, Ordering::Release);
            self.finished.store(false, Ordering::Relaxed);
            self.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Sample frames waiting in the queue.
    pub fn queued_frames(&self) -> usize {
        self.queue.lock().map(|q| q.len() / 2).unwrap_or(0)
    }

    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }

    /// Peak |sample| per channel since the last call (resets them).
    pub fn take_peaks(&self) -> [f32; 2] {
        [f32::from_bits(self.peak[0].swap(0, Ordering::Relaxed)), f32::from_bits(self.peak[1].swap(0, Ordering::Relaxed))]
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed) && self.queued_frames() == 0
    }
}

/// What a preview plays, in comp sample indices at the device rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaySpan {
    pub start: i64,
    /// Work area [wa, wb).
    pub wa: i64,
    pub wb: i64,
    pub looping: bool,
}

impl PlaySpan {
    /// Span for comp times: start, work area [wa, wb).
    pub fn new(start: Tick, wa: Tick, wb: Tick, looping: bool, rate: u32) -> PlaySpan {
        let r = rate as i64;
        PlaySpan { start: start.to_units_floor(r), wa: wa.to_units_floor(r), wb: wb.to_units_floor(r).max(wa.to_units_floor(r) + 1), looping }
    }
}

/// The comp sample index audible now: `consumed` frames played by the device minus its
/// `latency`, from the span start, wrapped into the work area when looping (clamped at its end
/// otherwise).
pub fn clock_sample(span: &PlaySpan, consumed: u64, latency: u64) -> i64 {
    clock_sample_at(span, consumed, latency, 1)
}

/// [`clock_sample`] at Play Faster's `speed`: each device frame is `speed` comp samples.
pub fn clock_sample_at(span: &PlaySpan, consumed: u64, latency: u64, speed: u32) -> i64 {
    let s = span.start.saturating_add((consumed.saturating_sub(latency) as i64).saturating_mul(i64::from(speed.max(1))));
    if s < span.wb {
        return s;
    }
    if span.looping { span.wa + (s - span.wb).rem_euclid((span.wb - span.wa).max(1)) } else { span.wb - 1 }
}

/// Comp time of sample index `s` at `rate` Hz.
pub fn sample_time(s: i64, rate: u32) -> Tick {
    Tick::from_units(s, rate.max(1) as i64)
}

/// The comp frame showing sample `s` (the frame whose interval contains it).
pub fn frame_of_sample(s: i64, rate: u32, fps: FrameRate) -> i64 {
    fps.frame_at(sample_time(s, rate))
}

/// Block size the feeder mixes at a time.
const BLOCK: usize = 1024;

/// A running audio preview: device + feeder thread.
pub struct AudioPlayback {
    pub feed: Arc<AudioFeed>,
    device: Box<dyn AudioDevice>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// wasm32 (no threads): what the feeder thread would own, run by [`AudioPlayback::pump`].
    feeder: Option<(RenderSource, ItemId, i64, u32, AudioStream, Option<Stretch>)>,
    /// Native: where the feeder thread continues after a hold ([`AudioPlayback::resume`]).
    seek: Arc<Mutex<Option<i64>>>,
    pub comp: ItemId,
    pub rate: u32,
    pub span: PlaySpan,
    /// Device frames consumed before the span (re)started at `span.start`: the clock counts
    /// from there.
    base: u64,
    /// Play Faster: comp samples per device frame (1 = normal speed).
    pub speed: u32,
}

/// Play Faster's pitch-keeping time compression at `speed` (none at normal speed).
fn stretch_for(rate: u32, speed: u32) -> Option<Stretch> {
    (speed > 1).then(|| Stretch::new(rate, f64::from(speed)))
}

/// Audio kept queued ahead of the device: ~200 ms (a feeder thread tops it up continuously);
/// wasm32 tops it up once per UI frame, so it keeps ~350 ms to ride out slow frames.
fn queue_target(rate: u32) -> usize {
    if cfg!(target_arch = "wasm32") { rate as usize * 7 / 20 } else { rate as usize / 5 }
}

impl AudioPlayback {
    /// Start mixing `comp` over `span` into `device`.
    /// `mix_rate`: Settings ▸ Audio ▸ Preview Sample Rate, the rate the comp's audio is mixed
    /// at (resampled to the device's rate when they differ; 0 = the device's rate).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        device: Box<dyn AudioDevice>,
        src: RenderSource,
        comp: ItemId,
        start: Tick,
        wa: Tick,
        wb: Tick,
        looping: bool,
        mix_rate: u32,
    ) -> Result<AudioPlayback, String> {
        Self::start_at_speed(device, src, comp, start, wa, wb, looping, mix_rate, 1)
    }

    /// [`Self::start`] at Play Faster's `speed` (×2, ×4, ×8): the sound keeps its pitch.
    #[allow(clippy::too_many_arguments)]
    pub fn start_at_speed(
        mut device: Box<dyn AudioDevice>,
        src: RenderSource,
        comp: ItemId,
        start: Tick,
        wa: Tick,
        wb: Tick,
        looping: bool,
        mix_rate: u32,
        speed: u32,
    ) -> Result<AudioPlayback, String> {
        let speed = speed.clamp(1, 16);
        let rate = device.sample_rate().max(8000);
        let mix = if mix_rate == 0 { rate } else { mix_rate.clamp(8000, 192_000) };
        let span = PlaySpan::new(start, wa, wb, looping, rate);
        let feed = Arc::new(AudioFeed::default());
        let stop = Arc::new(AtomicBool::new(false));
        if cfg!(target_arch = "wasm32") {
            // The device takes samples only in `pump`, which fills the queue first.
            let mut p = AudioPlayback {
                feed: feed.clone(),
                device,
                stop,
                thread: None,
                feeder: Some((src, comp, span.start, mix, AudioStream::default(), stretch_for(rate, speed))),
                seek: Arc::default(),
                comp,
                rate,
                span,
                base: 0,
                speed,
            };
            p.device.start(feed)?;
            p.pump();
            return Ok(p);
        }
        // All mixing happens on the feeder thread, the first block too: reading footage for the
        // first time opens it (a FLAC or MP3 file decodes whole), which froze the UI for as long
        // as that took (#408). The device plays silence until the first block arrives, and the
        // clock doesn't count it.
        let seek: Arc<Mutex<Option<i64>>> = Arc::default();
        let thread = {
            let (feed, stop, seek) = (feed.clone(), stop.clone(), seek.clone());
            std::thread::Builder::new()
                .name("ec-audio-feed".into())
                .spawn(move || {
                    let mut cursor = span.start;
                    let target = queue_target(rate);
                    // Keeps layer audio effects running from block to block (no pre-roll per block).
                    let mut stream = AudioStream::default();
                    let mut stretch = stretch_for(rate, speed);
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(to) = seek.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
                            // Resumed elsewhere: effect chains (and the time compression) start
                            // over there.
                            cursor = to;
                            stream = AudioStream::default();
                            stretch = stretch_for(rate, speed);
                        }
                        if !feed.held.load(Ordering::Acquire) && feed.queued_frames() < target && !feed.finished.load(Ordering::Relaxed) {
                            cursor = feed_block(&src, comp, &feed, &span, cursor, rate, mix, &stream, &mut stretch);
                        } else {
                            std::thread::sleep(std::time::Duration::from_millis(4));
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        device.start(feed.clone())?;
        Ok(AudioPlayback { feed, device, stop, thread: Some(thread), feeder: None, seek, comp, rate, span, base: 0, speed })
    }

    /// Rendering fell behind: silence, keeping the device open (reopening it on the UI thread
    /// stuttered at every uncached frame). The clock stops; [`Self::resume`] goes on.
    pub fn hold(&mut self) {
        self.feed.restart(true);
    }

    pub fn held(&self) -> bool {
        self.feed.held.load(Ordering::Acquire)
    }

    /// Play on from comp time `t` after [`Self::hold`]: the clock reads `t` until the device
    /// plays the first new sample.
    pub fn resume(&mut self, t: Tick) {
        let start = t.to_units_floor(self.rate as i64);
        // Outside the work area (cannot happen while it plays): from its start.
        let start = if start >= self.span.wb || start < self.span.wa { self.span.wa } else { start };
        self.span.start = start;
        if let Some((_, _, cursor, _, stream, stretch)) = &mut self.feeder {
            *cursor = start;
            *stream = AudioStream::default();
            *stretch = stretch_for(self.rate, self.speed);
        }
        *self.seek.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(start);
        // Everything handed to the device so far (the last buffers before the hold may still
        // be playing) belongs to the old position.
        self.base = self.feed.consumed();
        self.feed.restart(false);
    }

    /// Every UI frame while playing: on wasm32 mix ahead into the feed (what the feeder thread
    /// does elsewhere), then let the device take what it needs.
    pub fn pump(&mut self) {
        if let Some((src, comp, cursor, mix, stream, stretch)) = &mut self.feeder {
            let target = queue_target(self.rate);
            while !self.feed.held.load(Ordering::Acquire) && self.feed.queued_frames() < target && !self.feed.finished.load(Ordering::Relaxed) {
                *cursor = feed_block(src, *comp, &self.feed, &self.span, *cursor, self.rate, *mix, stream, stretch);
            }
        }
        self.device.pump();
    }

    /// The comp sample index audible now.
    pub fn clock(&self) -> i64 {
        match self.device.audible_frames() {
            Some(played) => clock_sample_at(&self.span, played.saturating_sub(self.base), 0, self.speed),
            None => clock_sample_at(&self.span, self.feed.consumed().saturating_sub(self.base), self.device.latency_frames(), self.speed),
        }
    }

    /// Seconds from the device's start until this preview's first sound was audible
    /// ([`AudioDevice::first_sound`]).
    pub fn first_sound(&self) -> Option<f64> {
        self.device.first_sound()
    }

    pub fn finished(&self) -> bool {
        // Consumed means submitted to the device, not heard. Let its final real samples drain
        // before stopping a non-looping preview (and cutting the hardware stream).
        self.feed.finished() && self.device.audible_frames().is_none_or(|played| played >= self.feed.consumed())
    }

    pub fn stop(&mut self) {
        self.device.stop();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mix one block at `cursor` into the feed; returns the next cursor (wrapping or finishing at
/// the work-area end). With `stretch` (Play Faster) the block is that many times longer and
/// goes in compressed in time: a loop's wrap goes through it too, so the device frames keep
/// counting comp samples at the speed.
#[allow(clippy::too_many_arguments)]
fn feed_block(
    src: &RenderSource,
    comp: ItemId,
    feed: &AudioFeed,
    span: &PlaySpan,
    cursor: i64,
    rate: u32,
    mix: u32,
    stream: &AudioStream,
    stretch: &mut Option<Stretch>,
) -> i64 {
    let mut cursor = cursor;
    if cursor >= span.wb {
        if !span.looping {
            feed.finished.store(true, Ordering::Relaxed);
            return cursor;
        }
        cursor = span.wa;
    }
    let per = stretch.as_ref().map_or(1, |s| s.speed().round() as usize).max(1);
    let n = ((span.wb - cursor) as usize).min(BLOCK.saturating_mul(per));
    let epoch = feed.epoch.load(Ordering::Acquire);
    let block = mix_block(src, comp, sample_time(cursor, rate), n, rate, mix, Some(stream));
    let block = match stretch {
        Some(st) => st.process(&block),
        None => block,
    };
    if !feed.push_at(epoch, &block) {
        // Held or moved while mixing: the next block starts where playback resumes.
        return cursor;
    }
    cursor + n as i64
}

/// `n` stereo frames of `comp`'s mix from comp time `t` at the device `rate`, mixed at `mix` Hz
/// (resampled when they differ). `stream` continues the previous block's effect chains.
fn mix_block(src: &RenderSource, comp: ItemId, t: Tick, n: usize, rate: u32, mix: u32, stream: Option<&AudioStream>) -> Vec<f32> {
    resampled(
        |at: Tick, frames: usize, r: u32| match stream {
            Some(s) => s.mix(&src.project, src.footage.as_ref(), src.expr.as_deref(), comp, at, frames, r),
            None => effectcraft_engine::render::audio::mix_comp(&src.project, src.footage.as_ref(), src.expr.as_deref(), comp, at, frames, r),
        },
        t,
        n,
        rate,
        mix,
    )
}

/// `n` frames at `rate` from comp time `t` of a mix rendered by `render(start, frames, rate)` at
/// `mix` Hz. `t` mostly falls between two samples at the mix rate: the block is interpolated
/// from the sample before it at that phase, so consecutive blocks join without a jump. Starting
/// each block on that sample instead shifted every block by up to a sample, a click every
/// block (#335: 44.1 kHz devices crackled).
fn resampled(render: impl Fn(Tick, usize, u32) -> Vec<f32>, t: Tick, n: usize, rate: u32, mix: u32) -> Vec<f32> {
    if mix == rate {
        return render(t, n, rate);
    }
    let tps = i128::from(effectcraft_engine::time::TICKS_PER_SECOND);
    let pos = i128::from(t.0).saturating_mul(i128::from(mix));
    let first = i64::try_from(pos.div_euclid(tps)).unwrap_or(0);
    let phase = pos.rem_euclid(tps) as f64 / tps as f64;
    let step = mix as f64 / rate.max(1) as f64;
    let m = (phase + n as f64 * step).ceil() as usize + 2;
    resample_stereo_at(&render(Tick::from_units(first, mix as i64), m, mix), mix, rate, n, phase)
}

/// Audio scrubbing: an open output that plays a short, faded snippet of the comp's mix each time
/// the current time moves to another frame (each snippet replaces what is still queued, so the
/// sound follows the pointer without lagging behind).
pub struct AudioScrub {
    pub feed: Arc<AudioFeed>,
    device: Box<dyn AudioDevice>,
    pub rate: u32,
    /// The comp and frame last played (holding still repeats nothing).
    last: Option<(ItemId, i64)>,
    /// When it last played (UI seconds): an idle scrub closes the device.
    pub last_used: f64,
}

impl AudioScrub {
    pub fn open(mut device: Box<dyn AudioDevice>, now: f64) -> Result<AudioScrub, String> {
        let rate = device.sample_rate().max(8000);
        let feed = Arc::new(AudioFeed::default());
        device.start(feed.clone())?;
        Ok(AudioScrub { feed, device, rate, last: None, last_used: now })
    }

    /// Play the frame of `comp` at `t` (one frame long, 30–100 ms) unless it was the last one
    /// played. Returns whether a snippet was queued.
    pub fn play(&mut self, src: &RenderSource, comp: ItemId, t: Tick, mix_rate: u32, now: f64) -> bool {
        self.last_used = now;
        let Some(c) = src.project.comp(comp) else { return false };
        let frame = c.frame_rate.frame_at(t);
        if self.last == Some((comp, frame)) {
            return false;
        }
        self.last = Some((comp, frame));
        let rate = self.rate;
        let mix = if mix_rate == 0 { rate } else { mix_rate.clamp(8000, 192_000) };
        let n = (c.frame_duration().seconds().clamp(0.03, 0.1) * rate as f64) as usize;
        let mut buf = mix_block(src, comp, c.frame_rate.tick_of(frame), n, rate, mix, None);
        // 3 ms fades: no clicks where snippets meet.
        let fade = (rate as usize * 3 / 1000).clamp(1, n / 2 + 1);
        let frames = buf.len() / 2;
        for i in 0..frames {
            let g = (i.min(frames - 1 - i) as f32 / fade as f32).min(1.0);
            buf[2 * i] *= g;
            buf[2 * i + 1] *= g;
        }
        self.feed.clear();
        self.feed.push(&buf);
        true
    }

    /// Every UI frame while open (Web Audio moves queued samples to the output here).
    pub fn pump(&mut self) {
        self.device.pump();
    }
}

impl Drop for AudioScrub {
    fn drop(&mut self) {
        self.device.stop();
    }
}

/// Hover-to-hear (Motion Library Sound cards): one decoded sound file on its own output, looping
/// while the pointer stays on the card, at a gain the panel's SFX preview volume changes while it
/// plays. A worker decodes the file and tops the feed up a block at a time (gain changes are heard
/// within [`SOUND_QUEUE_SECONDS`]); stopping fades out what is queued so nothing clicks. The output
/// stays open for a moment after the pointer leaves, so moving from card to card starts at once.
pub struct SoundPreview {
    pub feed: Arc<AudioFeed>,
    device: Box<dyn AudioDevice>,
    pub rate: u32,
    /// What plays (a card id) and its worker's stop flag.
    playing: Option<(String, Arc<AtomicBool>)>,
    /// The key whose sound couldn't be decoded (the panel says "Audio unavailable").
    failed: Arc<Mutex<Option<String>>>,
    /// Linear gain (f32 bits), read by the worker for every block.
    gain: Arc<AtomicU32>,
    /// When it last played (UI seconds): an idle output closes.
    pub last_used: f64,
}

/// Decodes a sound at the output's rate (interleaved stereo); runs on the preview's worker.
pub type SoundDecode = Box<dyn FnOnce(u32) -> Result<Vec<f32>, String> + Send>;

/// Hover sounds play at most this long (longer files are cut, then loop).
pub const SOUND_PREVIEW_MAX_SECONDS: f64 = 30.0;
/// Audio queued ahead of the device: how quickly a volume change is heard.
const SOUND_QUEUE_SECONDS: f64 = 0.08;
/// Fade when a sound stops or loops around (no clicks).
const SOUND_FADE_SECONDS: f64 = 0.005;

impl SoundPreview {
    pub fn open(mut device: Box<dyn AudioDevice>, now: f64) -> Result<SoundPreview, String> {
        let rate = device.sample_rate().max(8000);
        let feed = Arc::new(AudioFeed::default());
        device.start(feed.clone())?;
        Ok(SoundPreview { feed, device, rate, playing: None, failed: Arc::default(), gain: Arc::new(AtomicU32::new(1.0f32.to_bits())), last_used: now })
    }

    /// The key playing now.
    pub fn playing(&self) -> Option<&str> {
        self.playing.as_ref().map(|(k, _)| k.as_str())
    }

    /// The key whose sound failed to decode, if any.
    pub fn failed(&self) -> Option<String> {
        self.failed.lock().ok().and_then(|f| f.clone())
    }

    /// Linear gain 0–1 (applies to what is mixed from now on).
    pub fn set_gain(&self, gain: f32) {
        let g = if gain.is_finite() { gain.clamp(0.0, 1.0) } else { 0.0 };
        self.gain.store(g.to_bits(), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    /// Play the sound `decode` gives (looping while `looping`) under `key`, replacing what plays.
    pub fn play(&mut self, key: &str, decode: SoundDecode, looping: bool, now: f64) {
        self.last_used = now;
        self.stop();
        if let Ok(mut f) = self.failed.lock() {
            *f = None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.playing = Some((key.to_string(), stop.clone()));
        let epoch = self.feed.epoch.load(Ordering::Acquire);
        let (feed, gain, failed, rate, key) = (self.feed.clone(), self.gain.clone(), self.failed.clone(), self.rate, key.to_string());
        let work = move || {
            let pcm = match decode(rate) {
                Ok(v) if v.len() >= 2 => v,
                Ok(_) => fail(&failed, &key, &stop, "the sound is empty".into()),
                Err(e) => fail(&failed, &key, &stop, e),
            };
            if pcm.is_empty() {
                return;
            }
            feed_sound(&feed, epoch, &pcm, rate, &gain, &stop, looping);
        };
        // The worker sleeps between blocks: no hover sounds without threads (wasm32).
        if let Err(e) = std::thread::Builder::new().name("ec-sound-preview".into()).spawn(work) {
            log::warn!("sound preview: {e}");
            if let Ok(mut f) = self.failed.lock() {
                *f = self.playing.take().map(|(k, _)| k);
            }
        }
    }

    /// Stop what plays: what is queued fades out over a few milliseconds.
    pub fn stop(&mut self) {
        if let Some((_, stop)) = self.playing.take() {
            stop.store(true, Ordering::Relaxed);
            self.feed.fade_out((self.rate as f64 * SOUND_FADE_SECONDS) as usize);
        }
    }

    /// Every UI frame while open (Web Audio moves queued samples to the output here).
    pub fn pump(&mut self) {
        self.device.pump();
    }
}

impl Drop for SoundPreview {
    fn drop(&mut self) {
        if let Some((_, stop)) = self.playing.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.device.stop();
    }
}

/// A sound that didn't decode: remembered for the panel, nothing plays.
fn fail(failed: &Mutex<Option<String>>, key: &str, stop: &AtomicBool, e: String) -> Vec<f32> {
    if !stop.load(Ordering::Relaxed) {
        log::warn!("sound preview of {key}: {e}");
        if let Ok(mut f) = failed.lock() {
            *f = Some(key.to_string());
        }
    }
    Vec::new()
}

/// The worker's loop: keep [`SOUND_QUEUE_SECONDS`] queued, each block at the gain of the moment,
/// until stopped (or, not looping, at the end). Blocks queued for an older sound are refused by
/// the feed's epoch ([`AudioFeed::push_at`]).
fn feed_sound(feed: &AudioFeed, epoch: u64, pcm: &[f32], rate: u32, gain: &AtomicU32, stop: &AtomicBool, looping: bool) {
    let frames = pcm.len() / 2;
    let target = ((rate as f64 * SOUND_QUEUE_SECONDS) as usize).max(256);
    let block = (target / 4).max(64);
    let fade = ((rate as f64 * SOUND_FADE_SECONDS) as usize).clamp(1, frames / 2 + 1);
    let mut pos = 0usize;
    let mut buf = Vec::with_capacity(block * 2);
    while !stop.load(Ordering::Relaxed) {
        if feed.queued_frames() >= target {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        let g = f32::from_bits(gain.load(Ordering::Relaxed));
        let n = block.min(frames - pos);
        buf.clear();
        for i in pos..pos + n {
            // Looping: fade each pass in and out where it wraps around.
            let edge = if looping { (i.min(frames - 1 - i) as f32 / fade as f32).min(1.0) } else { 1.0 };
            let k = g * edge;
            buf.push(pcm.get(2 * i).copied().unwrap_or(0.0) * k);
            buf.push(pcm.get(2 * i + 1).copied().unwrap_or(0.0) * k);
        }
        if !feed.push_at(epoch, &buf) {
            break;
        }
        pos += n;
        if pos >= frames {
            if !looping {
                break;
            }
            pos = 0;
        }
    }
}

/// Decode a sound file's first [`SOUND_PREVIEW_MAX_SECONDS`] at `rate` Hz with FilmCraft's
/// codecs, in a pool of its own (no frame cache, nothing conformed to disk).
#[cfg(not(target_arch = "wasm32"))]
pub fn decode_sound_file(path: &str, rate: u32) -> Result<Vec<f32>, String> {
    let footage = effectcraft_media::probe(path).map_err(|e| e.to_string())?;
    if !footage.has_audio {
        return Err("the file has no audio".into());
    }
    let secs = footage.duration.seconds();
    let secs = if secs.is_finite() && secs > 0.0 { secs.min(SOUND_PREVIEW_MAX_SECONDS) } else { return Err("the sound is empty".into()) };
    let frames = (secs * rate as f64).ceil() as usize;
    let pcm = effectcraft_media::MediaPool::with_budget(0).audio_samples(&footage, Tick::ZERO, frames, rate);
    // The pool answers silence when a read fails.
    if pcm.iter().all(|v| *v == 0.0) {
        return Err("the sound couldn't be decoded".into());
    }
    Ok(pcm)
}

/// The web has no sound files to read.
#[cfg(target_arch = "wasm32")]
pub fn decode_sound_file(_path: &str, _rate: u32) -> Result<Vec<f32>, String> {
    Err("sound files can't be read here".into())
}

/// Linear resampling of interleaved stereo from `from` Hz to `n` frames at `to` Hz.
pub fn resample_stereo(src: &[f32], from: u32, to: u32, n: usize) -> Vec<f32> {
    resample_stereo_at(src, from, to, n, 0.0)
}

/// [`resample_stereo`] from `phase` (0..1) of a source frame past the first.
fn resample_stereo_at(src: &[f32], from: u32, to: u32, n: usize, phase: f64) -> Vec<f32> {
    let frames = src.len() / 2;
    let mut out = Vec::with_capacity(n * 2);
    if frames == 0 {
        out.resize(n * 2, 0.0);
        return out;
    }
    let step = from as f64 / to.max(1) as f64;
    for i in 0..n {
        let x = phase + i as f64 * step;
        let a = (x.floor() as usize).min(frames - 1);
        let b = (a + 1).min(frames - 1);
        let f = (x - a as f64) as f32;
        for c in 0..2 {
            let at = |k: usize| src.get(k * 2 + c).copied().unwrap_or(0.0);
            out.push(at(a) * (1.0 - f) + at(b) * f);
        }
    }
    out
}

/// VU meter state for the Audio panel: smoothed level with a falling decay and peak hold, in
/// dBFS per channel.
#[derive(Clone, Copy, Debug)]
pub struct Meter {
    pub level_db: [f32; 2],
    pub peak_db: [f32; 2],
    peak_at: [f64; 2],
    /// Clip indicators (latched until clicked / playback restarts).
    pub clipped: [bool; 2],
    last: f64,
}

impl Default for Meter {
    fn default() -> Self {
        Meter { level_db: [METER_FLOOR; 2], peak_db: [METER_FLOOR; 2], peak_at: [0.0; 2], clipped: [false; 2], last: 0.0 }
    }
}

/// Bottom of the meter scale (dB).
pub const METER_FLOOR: f32 = -48.0;
/// How long the peak marker holds (s) and how fast levels fall (dB/s).
const PEAK_HOLD: f64 = 1.5;
const FALL_DB_PER_S: f32 = 30.0;

pub fn to_db(v: f32) -> f32 {
    if v <= 1e-6 { METER_FLOOR } else { (20.0 * v.log10()).max(METER_FLOOR) }
}

impl Meter {
    /// Feed the peaks measured since the last update at wall time `now` (seconds).
    pub fn update(&mut self, peaks: [f32; 2], now: f64) {
        let dt = if self.last > 0.0 { (now - self.last).clamp(0.0, 0.5) as f32 } else { 0.0 };
        self.last = now;
        for c in 0..2 {
            let db = to_db(peaks[c]);
            self.level_db[c] = db.max(self.level_db[c] - FALL_DB_PER_S * dt);
            if db >= self.peak_db[c] || now - self.peak_at[c] > PEAK_HOLD {
                self.peak_db[c] = db.max(self.level_db[c]);
                self.peak_at[c] = now;
            }
            if peaks[c] >= 1.0 {
                self.clipped[c] = true;
            }
        }
    }

    /// Anything to animate (levels still falling)?
    pub fn active(&self) -> bool {
        self.level_db.iter().chain(self.peak_db.iter()).any(|d| *d > METER_FLOOR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_audible_cursor_drives_preview_and_final_buffer_drain() {
        struct Timestamped(Arc<AtomicU64>);
        impl AudioDevice for Timestamped {
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn start(&mut self, _: Arc<AudioFeed>) -> Result<(), String> {
                Ok(())
            }
            fn stop(&mut self) {}
            fn audible_frames(&self) -> Option<u64> {
                Some(self.0.load(Ordering::Relaxed))
            }
        }
        let heard = Arc::new(AtomicU64::new(1600));
        let feed = Arc::new(AudioFeed::default());
        feed.consumed.store(4096, Ordering::Relaxed);
        feed.finished.store(true, Ordering::Relaxed);
        let mut playback = AudioPlayback {
            feed,
            device: Box::new(Timestamped(heard.clone())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            feeder: None,
            seek: Arc::default(),
            comp: ItemId(1),
            rate: 48_000,
            span: PlaySpan { start: 96_000, wa: 48_000, wb: 144_000, looping: false },
            base: 0,
            speed: 1,
        };
        assert_eq!(playback.clock(), 97_600, "submitted samples do not jump the audible cursor");
        assert!(!playback.finished(), "the final hardware buffer is still playing");
        heard.store(4096, Ordering::Relaxed);
        assert!(playback.finished());
        playback.span = PlaySpan { start: 0, wa: 0, wb: 3200, looping: true };
        assert_eq!(playback.clock(), 896, "sample interpolation still uses the existing loop mapping");
    }

    /// B: a stall holds the sound instead of closing the device; resuming plays on from the
    /// frame on screen, the clock counting from there, and a block mixed for the old position
    /// while it held is never queued.
    #[test]
    fn hold_and_resume_keep_the_device_and_restart_the_clock() {
        struct Open(Arc<AtomicBool>);
        impl AudioDevice for Open {
            fn sample_rate(&self) -> u32 {
                48_000
            }
            fn start(&mut self, _: Arc<AudioFeed>) -> Result<(), String> {
                Ok(())
            }
            fn stop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let mut p = AudioPlayback {
            feed: Arc::new(AudioFeed::default()),
            device: Box::new(Open(stopped.clone())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            feeder: None,
            seek: Arc::default(),
            comp: ItemId(1),
            rate: 48_000,
            span: PlaySpan { start: 0, wa: 0, wb: 480_000, looping: true },
            base: 0,
            speed: 1,
        };
        p.feed.push(&[0.5; 9600]);
        let mut out = vec![0.0f32; 9600];
        p.feed.pull(&mut out);
        assert_eq!(p.clock(), 4800);
        p.feed.push(&[0.5; 2000]);
        let epoch = p.feed.epoch.load(Ordering::Acquire);
        p.hold();
        assert!(p.held() && p.feed.queued_frames() == 0, "what was queued for later is dropped");
        assert!(!p.feed.push_at(epoch, &[0.5; 2]), "a block mixed before the hold is not queued");
        p.feed.pull(&mut out);
        assert!(out.iter().all(|v| *v == 0.0), "silence while held");
        assert_eq!(p.feed.consumed(), 4800, "silence does not count");
        p.resume(Tick::from_seconds_f64(2.0));
        assert!(!p.held() && !stopped.load(Ordering::Relaxed), "the device stayed open");
        assert_eq!(*p.seek.lock().unwrap(), Some(96_000), "the feeder goes on from the frame on screen");
        assert_eq!(p.clock(), 96_000, "the clock holds there until new sound plays");
        assert!(p.feed.push_at(p.feed.epoch.load(Ordering::Acquire), &[0.5; 3200]));
        p.feed.pull(&mut out[..3200]);
        assert_eq!(p.clock(), 96_000 + 1600);
        // Resuming outside the work area starts at its beginning.
        p.hold();
        p.resume(Tick::from_seconds_f64(60.0));
        assert_eq!(p.clock(), 0);
    }

    /// #335: a mix at 48 kHz resampled to a 44.1 kHz device block by block. Each block began
    /// on the 48 kHz sample at or before its start, up to a sample early: a jump at every block
    /// boundary, heard as crackle. Resampling a ramp (the time of each sample) must give each
    /// output sample's own time, across block joins.
    #[test]
    fn resampled_blocks_join_without_a_jump() {
        let (rate, mix) = (44_100u32, 48_000u32);
        let ramp = |at: Tick, frames: usize, r: u32| {
            let s0 = at.to_units_floor(r as i64);
            (0..frames).flat_map(|i| [(s0 + i as i64) as f32 / r as f32; 2]).collect::<Vec<f32>>()
        };
        let mut cursor = 44_100 * 3 + 17;
        for _ in 0..8 {
            let out = resampled(ramp, sample_time(cursor, rate), BLOCK, rate, mix);
            assert_eq!(out.len(), BLOCK * 2);
            for (i, v) in out.chunks(2).enumerate() {
                let want = (cursor + i as i64) as f64 / rate as f64;
                assert!((v[0] as f64 - want).abs() < 2e-6, "sample {} of the block at {cursor}: {} vs {want}", i, v[0]);
            }
            cursor += BLOCK as i64;
        }
        // The same rate passes straight through.
        assert_eq!(resampled(ramp, sample_time(5, rate), 3, rate, rate), ramp(sample_time(5, rate), 3, rate));
    }

    #[test]
    fn clock_follows_consumed_frames_and_loops() {
        let span = PlaySpan { start: 4800, wa: 0, wb: 48000, looping: true };
        assert_eq!(clock_sample(&span, 0, 0), 4800);
        // Latency holds the clock back until the device has played through its buffer.
        assert_eq!(clock_sample(&span, 512, 1024), 4800);
        assert_eq!(clock_sample(&span, 2048, 1024), 4800 + 1024);
        // Wrap at the work-area end.
        assert_eq!(clock_sample(&span, 48000 - 4800 + 100, 0), 100);
        let once = PlaySpan { looping: false, ..span };
        assert_eq!(clock_sample(&once, 100_000, 0), 47999);
        // Play Faster ×4: each device frame is four comp samples.
        assert_eq!(clock_sample_at(&span, 1000, 0, 4), 4800 + 4000);
    }

    #[test]
    fn frame_mapping_matches_comp_frames() {
        let fps = FrameRate::new(30, 1);
        // 48 kHz at 30 fps: 1600 samples per frame.
        assert_eq!(frame_of_sample(0, 48000, fps), 0);
        assert_eq!(frame_of_sample(1599, 48000, fps), 0);
        assert_eq!(frame_of_sample(1600, 48000, fps), 1);
        assert_eq!(frame_of_sample(48000 * 2, 48000, fps), 60);
        let span = PlaySpan::new(Tick::from_seconds_f64(1.0), Tick::ZERO, Tick::from_seconds_f64(2.0), true, 48000);
        assert_eq!((span.start, span.wa, span.wb), (48000, 0, 96000));
        // 29.97: frame boundaries are fractional in samples.
        let ntsc = FrameRate::new(30000, 1001);
        assert_eq!(frame_of_sample(1601, 48000, ntsc), 0);
        assert_eq!(frame_of_sample(1602, 48000, ntsc), 1);
    }

    #[test]
    fn feed_counts_real_frames_and_peaks() {
        let f = AudioFeed::default();
        f.push(&[0.5, -0.25, 0.1, 0.9]);
        let mut out = [1.0f32; 8];
        f.pull(&mut out);
        assert_eq!(out, [0.5, -0.25, 0.1, 0.9, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(f.consumed(), 2);
        assert_eq!(f.take_peaks(), [0.5, 0.9]);
        assert_eq!(f.take_peaks(), [0.0, 0.0]);
    }

    /// #408: starting a preview with sound returns at once even when reading the footage takes
    /// long (the first read of a long FLAC decodes all of it): the first block was mixed on the
    /// calling (UI) thread. The sound arrives from the feeder thread, and the clock waits for it.
    #[test]
    fn starting_audio_does_not_wait_for_slow_footage() {
        use effectcraft_engine::color::Label;
        use effectcraft_engine::project::{Comp, Footage, FootageKind, ItemKind, LayerSource, Project, build};
        use effectcraft_engine::render::FootageSource;
        use effectcraft_engine::render::cache::LayerCache;
        use std::time::{Duration, Instant};

        /// Footage audio that takes `delay` to read the first time (then a constant 0.5).
        struct Slow {
            delay: Duration,
            opened: Mutex<bool>,
        }
        impl FootageSource for Slow {
            fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<effectcraft_engine::raster::Image>> {
                None
            }
            fn audio(&self, _: ItemId, _: &Footage, _: Tick, frames: usize, _: u32) -> Option<Vec<f32>> {
                let mut opened = self.opened.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if !*opened {
                    std::thread::sleep(self.delay);
                    *opened = true;
                }
                Some(vec![0.5; frames * 2])
            }
        }
        /// An output that only takes the feed.
        struct Device;
        impl AudioDevice for Device {
            fn sample_rate(&self) -> u32 {
                48000
            }
            fn start(&mut self, _: Arc<AudioFeed>) -> Result<(), String> {
                Ok(())
            }
            fn stop(&mut self) {}
        }

        let mut p = Project::default();
        let footage =
            Footage { path: "long.flac".into(), kind: FootageKind::Audio, duration: Tick::from_seconds_f64(600.0), has_audio: true, ..Default::default() };
        let item = p.add_item("long.flac", Label::SeaFoam, None, ItemKind::Footage(footage));
        let mut comp = Comp::new(64, 36, FrameRate::new(30, 1), Tick::from_seconds_f64(10.0));
        let layer = build::layer(&mut p, &comp, "long.flac", LayerSource::Footage { item }, (0, 0), None);
        comp.layers = vec![layer].into();
        let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.into()));
        let src = RenderSource {
            freeze_feedback: Default::default(),
            project: Arc::new(p),
            footage: Arc::new(Slow { delay: Duration::from_secs(2), opened: Mutex::new(false) }),
            expr: None,
            layer_cache: Arc::new(LayerCache::default()),
            gpu: None,
            gpu_display: false,
            disk: None,
        };
        let t = Instant::now();
        let mut playback =
            AudioPlayback::start(Box::new(Device), src, cid, Tick::ZERO, Tick::ZERO, Tick::from_seconds_f64(10.0), true, 0).expect("the preview starts");
        assert!(t.elapsed() < Duration::from_secs(1), "start waited {:?} for the footage", t.elapsed());
        assert_eq!(playback.clock(), 0, "the clock waits for the sound");
        // The feeder thread delivers it once the footage is open.
        while playback.feed.queued_frames() == 0 {
            assert!(t.elapsed() < Duration::from_secs(30), "the feeder never delivered");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut out = [0.0f32; 8];
        playback.feed.pull(&mut out);
        assert_eq!(out, [0.5; 8]);
        playback.stop();
    }

    /// An output that keeps the feed it is given.
    #[derive(Clone, Default)]
    struct Kept(Arc<Mutex<Option<Arc<AudioFeed>>>>);

    impl AudioDevice for Kept {
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn start(&mut self, feed: Arc<AudioFeed>) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(feed);
            Ok(())
        }
        fn stop(&mut self) {}
    }

    fn wait_for(what: &str, ok: impl Fn() -> bool) {
        let t = std::time::Instant::now();
        while !ok() {
            assert!(t.elapsed() < std::time::Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn sound_preview_loops_at_its_gain_and_fades_out_when_stopped() {
        let mut sp = SoundPreview::open(Box::new(Kept::default()), 0.0).unwrap();
        let feed = sp.feed.clone();
        sp.set_gain(0.5);
        // 1000 frames of a constant 0.8, looping.
        sp.play("a", Box::new(|rate| if rate == 48_000 { Ok(vec![0.8; 2000]) } else { Err(format!("rate {rate}")) }), true, 0.0);
        assert_eq!(sp.playing(), Some("a"));
        wait_for("three passes", || feed.queued_frames() >= 3000);
        let mut out = vec![0.0f32; 6000];
        feed.pull(&mut out);
        // At the gain; each pass fades in and out where it wraps (no clicks).
        assert!((out[1000] - 0.4).abs() < 1e-6, "{}", out[1000]);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[2000], 0.0, "the second pass starts faded in");
        assert!((out[2500] - 0.4).abs() < 1e-6);
        // A new volume is heard once what's queued has played.
        sp.set_gain(1.0);
        let mut drain = vec![0.0f32; feed.queued_frames() * 2 + 2048];
        feed.pull(&mut drain);
        wait_for("louder blocks", || feed.queued_frames() >= 2000);
        let mut out = vec![0.0f32; 4000];
        feed.pull(&mut out);
        assert!(out.iter().any(|v| (v - 0.8).abs() < 1e-6), "the new gain plays");
        // Stopping keeps only a short fade of what's queued; the worker queues nothing more.
        sp.stop();
        assert!(sp.playing().is_none());
        let fade = (48_000.0 * SOUND_FADE_SECONDS) as usize;
        assert!(feed.queued_frames() <= fade);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(feed.queued_frames() <= fade, "nothing queued after the stop");
        let mut tail = vec![0.0f32; fade * 2];
        feed.pull(&mut tail);
        assert!(tail.iter().rev().take(2).all(|v| v.abs() < 1e-6), "faded to silence");
    }

    #[test]
    fn sound_preview_reports_a_sound_that_does_not_decode() {
        let mut sp = SoundPreview::open(Box::new(Kept::default()), 0.0).unwrap();
        sp.play("bad", Box::new(|_| Err("unsupported".into())), true, 0.0);
        wait_for("the failure", || sp.failed().as_deref() == Some("bad"));
        assert_eq!(sp.feed.queued_frames(), 0);
        // Another sound clears it.
        sp.play("good", Box::new(|_| Ok(vec![0.5; 200])), false, 0.0);
        assert!(sp.failed().is_none());
        wait_for("the good sound", || sp.feed.queued_frames() > 0);
    }

    /// A 16-bit PCM WAV of a 440 Hz sine (stereo, `secs` long at `rate`).
    pub(crate) fn sine_wav(path: &std::path::Path, rate: u32, secs: f64, amp: f32) {
        let n = (rate as f64 * secs) as u32;
        let mut b = Vec::new();
        let data = n * 4;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 4).to_le_bytes());
        b.extend_from_slice(&4u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data.to_le_bytes());
        for i in 0..n {
            let v = ((i as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * amp * 32767.0) as i16;
            b.extend_from_slice(&v.to_le_bytes());
            b.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn decode_sound_file_reads_a_wav_at_the_output_rate() {
        let dir = std::env::temp_dir().join(format!("ec-sound-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("hit.wav");
        sine_wav(&wav, 44_100, 0.25, 0.5);
        let pcm = decode_sound_file(&wav.to_string_lossy(), 48_000).unwrap();
        assert_eq!(pcm.len(), 12_000 * 2);
        let peak = pcm.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak - 0.5).abs() < 0.02, "peak {peak}");
        // Not a sound: an error, not a panic or silence.
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, "hello").unwrap();
        assert!(decode_sound_file(&txt.to_string_lossy(), 48_000).is_err());
        assert!(decode_sound_file(&dir.join("missing.wav").to_string_lossy(), 48_000).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meter_decays_and_holds_peaks() {
        let mut m = Meter::default();
        m.update([1.0, 0.5], 1.0);
        assert!((m.level_db[0] - 0.0).abs() < 1e-4 && (m.level_db[1] + 6.0206).abs() < 1e-3);
        assert!(m.clipped[0] && !m.clipped[1]);
        m.update([0.0, 0.0], 1.1);
        assert!((m.level_db[0] + 3.0).abs() < 1e-3, "{}", m.level_db[0]);
        assert_eq!(m.peak_db[0], 0.0, "peak holds");
        // After the hold time the peak marker follows the falling level.
        m.update([0.0, 0.0], 3.0);
        assert!(m.peak_db[0] < -10.0, "{}", m.peak_db[0]);
    }
}
