//! Audio preview output through cpal (the device and channel mapping chosen in Settings ▸ Audio;
//! the default output device when none is set or it is gone).
//!
//! **One output, kept open (AU1).** The device stream is opened once and keeps running (silent)
//! between previews: every [`CpalOut`] (preview playback, audio scrubbing, hover-to-hear) only
//! attaches its [`AudioFeed`] to the running stream on `start` and detaches it on `stop`. The
//! stream used to be built on every play and dropped on every stop. On a Bluetooth output (the
//! owner's WH-1000XM5) the headset then woke its audio link on each play: the first 2–3 seconds
//! were not heard, while the picture ran on (the clock counts samples the device took). Building
//! a stream also cost ~700 ms on the first play of a session (the first CoreAudio device query),
//! on the UI thread. The app warms the output ([`warm`]) while a comp with sound is open and the
//! user is active; it closes after [`IDLE_CLOSE`] unused (an open output keeps a Mac awake) and
//! when the system's default output changes (then the next use opens the new one).
//!
//! cpal streams are not `Send` on every platform, so the stream lives on a small dedicated thread
//! (`ec-audio-out`). The callback pulls interleaved stereo from each attached feed, sums them and
//! maps the mix onto the device's channels (mono: L+R average; extra channels: silence),
//! converting to the device's sample format.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use effectcraft_ui_egui::audio::{AudioDevice, AudioFeed, AudioOutput};

/// The output closes after this long with nothing attached and no [`warm`] (an open output
/// stream keeps the Mac from sleeping).
const IDLE_CLOSE: Duration = Duration::from_secs(5 * 60);

/// How often the output thread looks at idleness and the system's default device.
const CHECK_EVERY: Duration = Duration::from_millis(500);

/// [`open`] waits at most this long for the device to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// A failed open is tried again by [`open`] (a play) after this long, by [`warm`] after
/// [`WARM_RETRY`] (no thread a frame on a machine without an output).
const OPEN_RETRY: Duration = Duration::from_secs(2);
const WARM_RETRY: Duration = Duration::from_secs(30);

/// Feeds attached at once (preview, scrub and hover previews; more are refused).
const MAX_SLOTS: usize = 8;

/// A sample this quiet counts as silence for the first-sound measurement.
const SILENT: f32 = 1.0e-5;

/// Device callback period: about 10 ms, clamped so Pulse/PipeWire/ALSA does not open a
/// multi-hundred-millisecond default buffer that plays as silence at the start of preview (#554).
fn preferred_buffer_frames(rate: u32) -> u32 {
    (rate / 100).clamp(256, 2048)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the output is opened for: Settings ▸ Audio's device (empty: the system default), the
/// 1-based left and right channels and the preferred rate.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    device: String,
    left: u32,
    right: u32,
    rate: u32,
}

impl Key {
    fn of(out: &AudioOutput) -> Key {
        Key { device: out.device.clone(), left: out.left, right: out.right, rate: out.rate }
    }
}

/// One started [`CpalOut`]'s feed, attached to the running stream.
struct Slot {
    id: u64,
    feed: Arc<AudioFeed>,
    clock: Arc<Mutex<AudibleClock>>,
    /// When it was attached (since the output's origin).
    started: Duration,
    /// Nanoseconds + 1 from `started` until its first non-silent sample is audible (0: not yet).
    first_sound: Arc<AtomicU64>,
}

/// The running output's state, shared by its thread, its callback and every [`CpalOut`].
struct Shared {
    origin: Instant,
    slots: Mutex<Vec<Slot>>,
    /// The stream is gone (closed, or its device went away): nothing attaches any more.
    dead: AtomicBool,
    /// Milliseconds since `origin` of the last use (attach, detach, warm, a feed playing).
    used: AtomicU64,
}

impl Shared {
    fn new() -> Shared {
        Shared { origin: Instant::now(), slots: Mutex::new(Vec::with_capacity(MAX_SLOTS)), dead: AtomicBool::new(false), used: AtomicU64::new(0) }
    }

    fn touch(&self) {
        self.used.store(self.origin.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn idle(&self) -> Duration {
        self.origin.elapsed().saturating_sub(Duration::from_millis(self.used.load(Ordering::Relaxed)))
    }

    /// Attach `feed` (fails once the stream is gone or every slot is taken).
    fn attach(&self, id: u64, feed: Arc<AudioFeed>, clock: Arc<Mutex<AudibleClock>>, first_sound: Arc<AtomicU64>) -> Result<(), String> {
        let mut slots = lock(&self.slots);
        // Checked under the slots lock: the output thread decides to close under it too.
        if self.dead.load(Ordering::Acquire) {
            return Err("the audio output closed".into());
        }
        if slots.len() >= MAX_SLOTS {
            return Err("too many audio previews at once".into());
        }
        first_sound.store(0, Ordering::Relaxed);
        slots.push(Slot { id, feed, clock, started: self.origin.elapsed(), first_sound });
        drop(slots);
        self.touch();
        Ok(())
    }

    fn detach(&self, id: u64) {
        lock(&self.slots).retain(|s| s.id != id);
        self.touch();
    }

    /// Close the output if nothing is attached (decided under the slots lock, so an attach
    /// never lands on a stream that is closing). Returns whether it closed.
    fn close_if_unused(&self) -> bool {
        let slots = lock(&self.slots);
        if !slots.is_empty() {
            return false;
        }
        self.dead.store(true, Ordering::Release);
        true
    }
}

/// The output's open result: its sample rate, or why it did not open.
#[derive(Default)]
struct Ready {
    result: Mutex<Option<Result<u32, String>>>,
    cv: Condvar,
}

impl Ready {
    fn set(&self, r: Result<u32, String>) {
        *lock(&self.result) = Some(r);
        self.cv.notify_all();
    }

    fn wait(&self, timeout: Duration) -> Result<u32, String> {
        let g = lock(&self.result);
        let (g, _) = self.cv.wait_timeout_while(g, timeout, |r| r.is_none()).unwrap_or_else(PoisonError::into_inner);
        g.clone().unwrap_or_else(|| Err("the audio output did not open in time".into()))
    }
}

/// The output (one per process).
struct Engine {
    key: Key,
    shared: Arc<Shared>,
    ready: Arc<Ready>,
    born: Instant,
    /// Dropped when the engine is replaced: its thread closes once its feeds detach.
    _retire: mpsc::Sender<()>,
}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

static NEXT_SLOT: AtomicU64 = AtomicU64::new(1);

/// Whether the current engine serves `key`: it is open (or opening), or it failed less than
/// `retry` ago (not tried again yet).
fn reusable(engine: &Engine, key: &Key, retry: Duration) -> bool {
    engine.key == *key && (!engine.shared.dead.load(Ordering::Acquire) || engine.born.elapsed() < retry)
}

/// The output for `out`, opened (on its own thread) when there is none, it closed, or Settings
/// ▸ Audio changed. `None` when the thread cannot start.
fn engine(out: &AudioOutput, retry: Duration) -> Option<(Arc<Shared>, Arc<Ready>)> {
    let key = Key::of(out);
    let mut g = lock(&ENGINE);
    if let Some(e) = g.as_ref().filter(|e| reusable(e, &key, retry)) {
        e.shared.touch();
        return Some((e.shared.clone(), e.ready.clone()));
    }
    let shared = Arc::new(Shared::new());
    shared.touch();
    let ready = Arc::new(Ready::default());
    let (tx, rx) = mpsc::channel::<()>();
    let spawned = std::thread::Builder::new().name("ec-audio-out".into()).spawn({
        let (key, shared, ready) = (key.clone(), shared.clone(), ready.clone());
        move || run_output(key, shared, ready, rx)
    });
    if let Err(e) = spawned {
        log::warn!("audio output: {e}");
        return None;
    }
    *g = Some(Engine { key, shared: shared.clone(), ready: ready.clone(), born: Instant::now(), _retire: tx });
    Some((shared, ready))
}

/// Open the output in the background (no wait), or keep the open one from closing: the app
/// calls it while a comp with sound is open and the user is active, so the first play finds
/// the device (and a Bluetooth headset's audio link) awake.
pub fn warm(out: &AudioOutput) {
    let _ = engine(out, WARM_RETRY);
}

/// The output thread: opens the stream, then keeps it running until it is unused for
/// [`IDLE_CLOSE`], replaced, its device goes away, or the system default output changes.
fn run_output(key: Key, shared: Arc<Shared>, ready: Arc<Ready>, retire: mpsc::Receiver<()>) {
    let t0 = Instant::now();
    let (stream, rate, name) = match open_stream(&key, &shared) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("audio output: {e}");
            shared.dead.store(true, Ordering::Release);
            ready.set(Err(e));
            return;
        }
    };
    log::info!("audio output: {name} at {rate} Hz opened in {:.0} ms (kept open while EffectCraft is in use)", t0.elapsed().as_secs_f64() * 1000.0);
    ready.set(Ok(rate));
    let mut retired = false;
    loop {
        if retired {
            std::thread::sleep(CHECK_EVERY);
        } else {
            match retire.recv_timeout(CHECK_EVERY) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => retired = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        if !lock(&shared.slots).is_empty() {
            // Playing: in use, and never closed under a preview (its clock would stop).
            shared.touch();
            continue;
        }
        let why = if shared.dead.load(Ordering::Acquire) {
            Some("its device went away")
        } else if retired {
            Some("Settings ▸ Audio changed")
        } else if shared.idle() >= IDLE_CLOSE {
            Some("unused for 5 minutes")
        } else if key.device.is_empty() && default_output_name().is_some_and(|d| d != name) {
            Some("the system's default output changed")
        } else {
            None
        };
        if let Some(why) = why
            && shared.close_if_unused()
        {
            log::info!("audio output: {name} closed: {why}");
            break;
        }
    }
    drop(stream);
}

#[allow(deprecated)]
fn default_output_name() -> Option<String> {
    cpal::default_host().default_output_device().and_then(|d| d.name().ok())
}

/// Open and start the stream for `key`: its sample rate and device name.
fn open_stream(key: &Key, shared: &Arc<Shared>) -> Result<(cpal::Stream, u32, String), String> {
    let dev = device(&key.device).ok_or("no audio output device")?;
    #[allow(deprecated)]
    let name = dev.name().unwrap_or_default();
    let sup = dev.default_output_config().map_err(|e| e.to_string())?;
    // Settings ▸ Audio ▸ Preview Sample Rate: open the device at that rate when it supports it
    // (otherwise the preview is mixed at that rate and resampled to the device's).
    let supports = |r: u32| {
        dev.supported_output_configs()
            .map(|mut c| c.any(|c| c.channels() == sup.channels() && c.min_sample_rate().0 <= r && r <= c.max_sample_rate().0))
            .unwrap_or(false)
    };
    let rate = if key.rate > 0 && supports(key.rate) { key.rate } else { sup.sample_rate().0 };
    let mut cfg: cpal::StreamConfig = sup.config();
    cfg.sample_rate = cpal::SampleRate(rate);
    // A short period only where the default one is long (ALSA/PulseAudio on Linux, #554), and
    // only within what the device reports it takes.
    if cfg!(target_os = "linux") {
        let want = preferred_buffer_frames(rate);
        cfg.buffer_size = match sup.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } if *min <= *max => cpal::BufferSize::Fixed(want.clamp(*min, *max)),
            _ => cpal::BufferSize::Default,
        };
    }
    let map = ((key.left.max(1) - 1) as usize, (key.right.max(1) - 1) as usize);
    let build = |cfg: &cpal::StreamConfig| match sup.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(&dev, cfg, shared.clone(), map),
        cpal::SampleFormat::I16 => run::<i16>(&dev, cfg, shared.clone(), map),
        cpal::SampleFormat::U16 => run::<u16>(&dev, cfg, shared.clone(), map),
        cpal::SampleFormat::I32 => run::<i32>(&dev, cfg, shared.clone(), map),
        f => Err(format!("unsupported sample format {f:?}")),
    };
    let stream = match build(&cfg) {
        Ok(s) => s,
        Err(_) => {
            // Some hosts reject a fixed period; a large default still plays, just later.
            cfg.buffer_size = cpal::BufferSize::Default;
            build(&cfg)?
        }
    };
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, rate, name))
}

pub struct CpalOut {
    out: AudioOutput,
    rate: u32,
    shared: Arc<Shared>,
    clock: Arc<Mutex<AudibleClock>>,
    first_sound: Arc<AtomicU64>,
    /// The attached slot while started.
    slot: Option<u64>,
}

/// A submitted buffer's real prefix (an underrun adds silence after it, which never advances
/// the composition clock). Times are relative to the native output's monotonic origin.
struct AudibleInterval {
    start: Duration,
    first: u64,
    real: u64,
    rate: u32,
}

struct AudibleClock {
    intervals: VecDeque<AudibleInterval>,
    last: u64,
}

const CLOCK_INTERVALS: usize = 256;

impl Default for AudibleClock {
    fn default() -> Self {
        Self { intervals: VecDeque::with_capacity(CLOCK_INTERVALS), last: 0 }
    }
}

impl AudibleClock {
    fn reset(&mut self) {
        self.intervals.clear();
        self.last = 0;
    }

    fn stop(&mut self, now: Duration) {
        self.position(now);
        self.intervals.clear();
    }

    fn record(&mut self, now: Duration, delay: Duration, first: u64, real: u64, rate: u32) {
        self.position(now);
        // Never allocate in the callback. If a broken driver queues excessive future intervals,
        // retain the audible history and hold at its end until another callback can be recorded.
        if self.intervals.len() < CLOCK_INTERVALS {
            let start = now.saturating_add(delay);
            self.intervals.push_back(AudibleInterval { start, first, real, rate });
        }
    }

    fn position(&mut self, now: Duration) -> u64 {
        while self.intervals.get(1).is_some_and(|next| next.start <= now) {
            self.intervals.pop_front();
        }
        if let Some(interval) = self.intervals.front().filter(|interval| interval.start <= now) {
            let elapsed = now.saturating_sub(interval.start).as_nanos();
            let samples = elapsed.saturating_mul(u128::from(interval.rate)) / 1_000_000_000;
            let played = samples.min(u128::from(interval.real)) as u64;
            // Latency estimates may change between callbacks; never rewind audible samples.
            self.last = self.last.max(interval.first.saturating_add(played));
        }
        self.last
    }
}

/// Names of the output devices (Settings ▸ Audio).
pub fn devices() -> Vec<String> {
    let host = cpal::default_host();
    #[allow(deprecated)]
    host.output_devices().map(|d| d.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
}

/// The named output device, or the default one.
fn device(name: &str) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if !name.is_empty() {
        #[allow(deprecated)]
        let found = host.output_devices().ok().and_then(|mut d| d.find(|d| d.name().ok().as_deref() == Some(name)));
        if found.is_some() {
            return found;
        }
    }
    host.default_output_device()
}

/// An output for a preview (`None` without a device). The device stays open between previews
/// (see the module docs): only the first call of a session, or one after the output closed,
/// waits for it to open.
pub fn open(out: &AudioOutput) -> Option<Box<dyn AudioDevice>> {
    let (shared, ready) = engine(out, OPEN_RETRY)?;
    let rate = ready.wait(OPEN_TIMEOUT).ok()?;
    Some(Box::new(CpalOut {
        out: out.clone(),
        rate,
        shared,
        clock: Arc::new(Mutex::new(AudibleClock::default())),
        first_sound: Arc::new(AtomicU64::new(0)),
        slot: None,
    }))
}

/// Mix every attached feed into `mix` (interleaved stereo, `frames` = `mix.len() / 2`) for a
/// device buffer whose first sample is audible `delay` after `now`, recording each feed's real
/// samples in its clock and its first sound. `one` is scratch space (grown, never shrunk).
fn mix_slots(slots: &[Slot], mix: &mut [f32], one: &mut Vec<f32>, now: Duration, delay: Duration, rate: u32) {
    mix.iter_mut().for_each(|v| *v = 0.0);
    one.resize(mix.len(), 0.0);
    for s in slots {
        let first = s.feed.consumed();
        s.feed.pull(one);
        let real = s.feed.consumed().saturating_sub(first);
        for (m, v) in mix.iter_mut().zip(one.iter()) {
            *m += *v;
        }
        if s.first_sound.load(Ordering::Relaxed) == 0 {
            let n = usize::try_from(real).unwrap_or(usize::MAX).saturating_mul(2);
            if let Some(i) = one.iter().take(n).position(|v| v.abs() > SILENT) {
                let at = Duration::from_nanos(((i / 2) as u64).saturating_mul(1_000_000_000) / u64::from(rate.max(1)));
                let audible = now.saturating_add(delay).saturating_add(at).saturating_sub(s.started);
                s.first_sound.store(u64::try_from(audible.as_nanos()).unwrap_or(u64::MAX - 1).saturating_add(1), Ordering::Relaxed);
            }
        }
        lock(&s.clock).record(now, delay, first, real, rate);
    }
}

fn run<T: SizedSample + FromSample<f32>>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    shared: Arc<Shared>,
    map: (usize, usize),
) -> Result<cpal::Stream, String> {
    let ch = cfg.channels as usize;
    // Output mapping: device channels (0-based) for left and right; out of range → 1 and 2.
    let (left, right) = if map.0 < ch && map.1 < ch { map } else { (0, 1) };
    let rate = cfg.sample_rate.0;
    let mut stereo: Vec<f32> = Vec::new();
    let mut one: Vec<f32> = Vec::new();
    let gone = shared.clone();
    dev.build_output_stream(
        cfg,
        move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
            let now = shared.origin.elapsed();
            let frames = out.len() / ch.max(1);
            stereo.resize(frames * 2, 0.0);
            // CPAL's playback timestamp is when this buffer's FIRST sample becomes audible.
            // Keep earlier intervals too: this callback may run while an older buffer plays.
            let ts = info.timestamp();
            let delay = ts.playback.duration_since(&ts.callback).unwrap_or_default();
            mix_slots(&lock(&shared.slots), &mut stereo, &mut one, now, delay, rate);
            for (f, s) in out.chunks_mut(ch.max(1)).zip(stereo.as_chunks::<2>().0.iter()) {
                for (c, o) in f.iter_mut().enumerate() {
                    // Mono devices, or left and right mapped to the same channel, get the mix.
                    let v = if ch == 1 || (c == left && c == right) {
                        (s[0] + s[1]) * 0.5
                    } else if c == left {
                        s[0]
                    } else if c == right {
                        s[1]
                    } else {
                        0.0
                    };
                    *o = T::from_sample(v.clamp(-1.0, 1.0));
                }
            }
        },
        move |e| {
            log::warn!("audio output: {e}");
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                gone.dead.store(true, Ordering::Release);
            }
        },
        None,
    )
    .map_err(|e| e.to_string())
}

impl AudioDevice for CpalOut {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn start(&mut self, feed: Arc<AudioFeed>) -> Result<(), String> {
        self.stop();
        if self.shared.dead.load(Ordering::Acquire) {
            // Closed since `open` (idle, device change): the current output, at the same rate
            // (the preview was mixed for it).
            let (shared, ready) = engine(&self.out, OPEN_RETRY).ok_or("no audio output")?;
            let rate = ready.wait(OPEN_TIMEOUT)?;
            if rate != self.rate {
                return Err(format!("the audio output changed to {rate} Hz"));
            }
            self.shared = shared;
        }
        lock(&self.clock).reset();
        let id = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        self.shared.attach(id, feed, self.clock.clone(), self.first_sound.clone())?;
        self.slot = Some(id);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(id) = self.slot.take() {
            self.shared.detach(id);
        }
        lock(&self.clock).stop(self.shared.origin.elapsed());
    }

    fn audible_frames(&self) -> Option<u64> {
        Some(lock(&self.clock).position(self.shared.origin.elapsed()))
    }

    fn first_sound(&self) -> Option<f64> {
        match self.first_sound.load(Ordering::Relaxed) {
            0 => None,
            n => Some((n - 1) as f64 / 1e9),
        }
    }
}

impl Drop for CpalOut {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_buffer_is_about_ten_milliseconds() {
        assert_eq!(preferred_buffer_frames(48_000), 480);
        assert_eq!(preferred_buffer_frames(8_000), 256);
        assert_eq!(preferred_buffer_frames(192_000), 1920);
    }

    fn slot(shared: &Shared, feed: &Arc<AudioFeed>) -> (u64, Arc<Mutex<AudibleClock>>, Arc<AtomicU64>) {
        let id = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        let (clock, first) = (Arc::new(Mutex::new(AudibleClock::default())), Arc::new(AtomicU64::new(0)));
        shared.attach(id, feed.clone(), clock.clone(), first.clone()).unwrap();
        (id, clock, first)
    }

    /// AU1: a preview attaches to the output that is already running. Its first sound is heard
    /// in the next device buffer after the feeder's first block (not after a stream is built,
    /// nor after a Bluetooth headset wakes up), and the silent buffers before it never count.
    #[test]
    fn a_feed_attached_to_the_running_output_sounds_in_the_next_buffer() {
        let shared = Shared::new();
        let (mut mix, mut one) = (vec![0.0f32; 2 * 441], Vec::new());
        // The output runs silent (no preview) for a while.
        for k in 0..10u64 {
            mix_slots(&lock(&shared.slots), &mut mix, &mut one, Duration::from_millis(10 * k), Duration::from_millis(5), 44_100);
        }
        let feed = Arc::new(AudioFeed::default());
        let (_, clock, first) = slot(&shared, &feed);
        let started = lock(&shared.slots)[0].started;
        // A buffer before the feeder's first block: silence, nothing measured or counted.
        mix_slots(&lock(&shared.slots), &mut mix, &mut one, started + Duration::from_millis(1), Duration::from_millis(5), 44_100);
        assert_eq!(first.load(Ordering::Relaxed), 0);
        assert_eq!(feed.consumed(), 0);
        // The first block: 100 silent frames, then sound.
        let mut block = vec![0.0f32; 2 * 441];
        block[200..].iter_mut().for_each(|v| *v = 0.25);
        feed.push(&block);
        let now = started + Duration::from_millis(11);
        mix_slots(&lock(&shared.slots), &mut mix, &mut one, now, Duration::from_millis(5), 44_100);
        assert_eq!(feed.consumed(), 441);
        assert!(mix[200..].iter().all(|v| (*v - 0.25).abs() < 1e-6) && mix[..200].iter().all(|v| *v == 0.0));
        // 11 ms to the callback + 5 ms output delay + 100 frames at 44.1 kHz (2.27 ms).
        let heard = (first.load(Ordering::Relaxed) - 1) as f64 / 1e6;
        assert!((heard - 18.27).abs() < 0.05, "first sound {heard} ms after start");
        assert!(heard < 100.0);
        assert_eq!(lock(&clock).position(now + Duration::from_millis(5) + Duration::from_millis(10)), 441);
    }

    #[test]
    fn attached_feeds_sum_and_detach_without_closing_the_output() {
        let shared = Shared::new();
        let (a, b) = (Arc::new(AudioFeed::default()), Arc::new(AudioFeed::default()));
        let (ida, ..) = slot(&shared, &a);
        let (idb, clock_b, _) = slot(&shared, &b);
        a.push(&[0.1, 0.2, 0.1, 0.2]);
        b.push(&[0.3, -0.2]);
        let (mut mix, mut one) = (vec![0.0f32; 4], Vec::new());
        mix_slots(&lock(&shared.slots), &mut mix, &mut one, Duration::ZERO, Duration::ZERO, 48_000);
        assert!((mix[0] - 0.4).abs() < 1e-6 && mix[1].abs() < 1e-6 && (mix[2] - 0.1).abs() < 1e-6 && (mix[3] - 0.2).abs() < 1e-6);
        assert_eq!((a.consumed(), b.consumed()), (2, 1), "underrun silence is not counted");
        assert_eq!(lock(&clock_b).position(Duration::from_secs(1)), 1);
        shared.detach(ida);
        shared.detach(idb);
        assert!(lock(&shared.slots).is_empty());
        assert!(!shared.dead.load(Ordering::Relaxed), "stopping a preview keeps the output open");
        // Unused: the output thread may close it, and then nothing attaches to it any more.
        assert!(shared.close_if_unused());
        assert!(shared.attach(99, a, clock_b, Arc::new(AtomicU64::new(0))).is_err());
    }

    #[test]
    fn a_running_preview_keeps_the_output_from_closing() {
        let shared = Shared::new();
        let feed = Arc::new(AudioFeed::default());
        let (id, ..) = slot(&shared, &feed);
        assert!(!shared.close_if_unused());
        assert!(!shared.dead.load(Ordering::Relaxed));
        shared.detach(id);
        assert!(shared.close_if_unused());
    }

    #[test]
    fn the_output_is_reused_until_settings_change_or_it_closes() {
        let shared = Arc::new(Shared::new());
        let (tx, _rx) = mpsc::channel();
        let key = Key { device: String::new(), left: 1, right: 2, rate: 48_000 };
        let e = Engine { key: key.clone(), shared: shared.clone(), ready: Arc::default(), born: Instant::now(), _retire: tx };
        assert!(reusable(&e, &key, OPEN_RETRY));
        assert!(!reusable(&e, &Key { rate: 44_100, ..key.clone() }, OPEN_RETRY), "another preview rate opens again");
        assert!(!reusable(&e, &Key { device: "Speakers".into(), ..key.clone() }, OPEN_RETRY));
        shared.dead.store(true, Ordering::Relaxed);
        assert!(reusable(&e, &key, OPEN_RETRY), "a failed open is not retried at once");
        assert!(!reusable(&e, &key, Duration::ZERO), "a closed output opens again");
    }

    #[test]
    fn ready_waits_for_the_open_and_times_out() {
        let ready = Arc::new(Ready::default());
        assert!(ready.wait(Duration::from_millis(5)).is_err());
        let r = ready.clone();
        let h = std::thread::spawn(move || r.wait(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(10));
        ready.set(Ok(44_100));
        assert_eq!(h.join().unwrap(), Ok(44_100));
    }
}

#[cfg(test)]
mod clock_tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn cached_video_clock_advances_between_large_device_callbacks() {
        for frames in [2048, 4096] {
            let mut clock = AudibleClock::default();
            clock.record(Duration::ZERO, Duration::ZERO, 0, frames, 48_000);
            // No second device callback: a 30fps viewer can still show its second frame.
            assert_eq!(clock.position(ms(34)), 1632);
            assert!(clock.position(ms(34)) / 1600 == 1);
            assert_eq!(clock.position(ms(42)), 2016);
            // Never extrapolate beyond audio actually supplied to the device.
            assert_eq!(clock.position(ms(500)), frames);
        }
    }

    #[test]
    fn fully_cached_preview_has_no_callback_sized_frame_jumps() {
        for (numerator, denominator) in [(30u64, 1u64), (30_000, 1001)] {
            for rate in [44_100u64, 48_000] {
                for buffer in [2048u64, 4096] {
                    let mut clock = AudibleClock::default();
                    let mut next_sample = 0u64;
                    let mut last = 0u64;
                    let mut changed = 0;
                    for refresh in 0..=120u64 {
                        let now = Duration::from_nanos(refresh * 1_000_000_000 / 120 + 1000);
                        loop {
                            let callback = Duration::from_nanos(next_sample * 1_000_000_000 / rate);
                            if callback > now {
                                break;
                            }
                            clock.record(callback, Duration::ZERO, next_sample, buffer, rate as u32);
                            next_sample += buffer;
                        }
                        let frame = clock.position(now) * numerator / (rate * denominator);
                        assert!(frame <= last + 1, "{buffer}-sample callbacks at {rate} Hz skipped cached video: {last} -> {frame}");
                        if frame != last {
                            changed += 1;
                        }
                        last = frame;
                    }
                    assert_eq!(changed, numerator / denominator, "every cached frame due within one second can be shown");
                }
            }
        }
    }

    #[test]
    fn future_callbacks_keep_the_currently_audible_interval() {
        let mut clock = AudibleClock::default();
        // Three buffers submitted before the first is audible.
        clock.record(ms(0), ms(30), 0, 960, 48_000);
        clock.record(ms(10), ms(40), 960, 960, 48_000);
        clock.record(ms(20), ms(50), 1920, 960, 48_000);
        assert_eq!(clock.position(ms(25)), 0);
        assert_eq!(clock.position(ms(35)), 240);
        assert_eq!(clock.position(ms(45)), 720);
        assert_eq!(clock.position(ms(55)), 1200);
        assert_eq!(clock.position(ms(75)), 2160);
        assert_eq!(clock.position(ms(100)), 2880);
    }

    #[test]
    fn partial_and_complete_underruns_hold_after_the_real_prefix() {
        let mut clock = AudibleClock::default();
        clock.record(ms(0), ms(10), 0, 480, 48_000); // 10ms real audio, then silence
        clock.record(ms(1), ms(39), 480, 0, 48_000); // wholly silent buffer
        clock.record(ms(2), ms(68), 480, 960, 48_000); // audio resumes at 70ms
        assert_eq!(clock.position(ms(15)), 240);
        assert_eq!(clock.position(ms(30)), 480);
        assert_eq!(clock.position(ms(50)), 480);
        assert_eq!(clock.position(ms(69)), 480);
        assert_eq!(clock.position(ms(75)), 720);
        assert_eq!(clock.position(ms(1000)), 1440);
    }

    #[test]
    fn stop_seek_and_restart_do_not_reuse_a_previous_audio_cursor() {
        let mut clock = AudibleClock::default();
        clock.record(ms(0), ms(0), 0, 4800, 48_000);
        clock.stop(ms(20));
        assert_eq!(clock.position(ms(200)), 960, "stop holds the audible cursor");
        clock.reset();
        assert_eq!(clock.position(ms(200)), 0);
        clock.record(ms(200), ms(10), 0, 960, 48_000);
        assert_eq!(clock.position(ms(205)), 0);
        assert_eq!(clock.position(ms(215)), 240);
    }

    #[test]
    fn changing_latency_never_rewinds_and_future_history_is_bounded() {
        let mut clock = AudibleClock::default();
        clock.record(ms(0), ms(0), 0, 960, 48_000);
        assert_eq!(clock.position(ms(15)), 720);
        clock.record(ms(16), ms(40), 960, 960, 48_000);
        assert_eq!(clock.position(ms(40)), 960, "larger latency holds at the real prefix end");
        assert_eq!(clock.position(ms(60)), 1152);
        assert!(clock.position(ms(59)) >= 1152, "even a slightly older observation cannot rewind");
        clock.reset();
        let capacity = clock.intervals.capacity();
        for i in 0..1000 {
            clock.record(ms(0), ms(1000 + i), i * 48, 48, 48_000);
        }
        assert_eq!(clock.intervals.len(), CLOCK_INTERVALS);
        assert_eq!(clock.intervals.capacity(), capacity, "callback recording never grows the allocation");
        assert_eq!(clock.position(ms(500)), 0, "future buffers are not audible yet");
    }
}
