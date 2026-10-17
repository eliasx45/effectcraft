//! [`Backend::Auto`](crate::Backend::Auto)'s per-comp choice between the CPU and the GPU
//! compositor, from measured frame times.
//!
//! A light comp (a couple of small layers) composites faster on the CPU, which only touches
//! the layers' bounds, than on the GPU, which works on whole frames and reads the result back;
//! heavy comps (many layers, 3D, GPU effects) are faster on the GPU. Neither a layer count nor
//! a pixel count predicts the crossover across machines, so Auto measures: per comp, output
//! scale and path (readback / viewer display), it keeps an exponentially weighted average of
//! the CPU and GPU frame times and renders on the faster one. Each side is warmed up first
//! (its first frame, with layer cache misses and texture uploads, is not counted; the fastest of
//! the next ones starts the average), and the slower side is re-measured every
//! [`AutoPick::REPROBE`] frames so the choice follows the comp as it changes.
//!
//! A CPU frame in a comp the GPU renders is costly beyond its own time: the layers the GPU
//! keeps on the device are read back one by one, a hitch playback waits for. So while the GPU is
//! clearly faster ([`AutoPick::CLEAR`]) the CPU is re-measured less and less often (up to
//! [`AutoPick::MAX_REPROBE`] frames apart), and a new key (the same comp at another resolution,
//! when adaptive resolution steps) whose comp already measured the GPU clearly faster starts on
//! the GPU without CPU warm-up frames, re-measuring the CPU at the first re-probe.
//!
//! A playing preview asks with a budget ([`AutoPick::choose_within`], a frame's duration): it
//! shows frames in order, so one slow CPU frame holds up all the frames after it. Its frames go
//! to the CPU only when the CPU measured within the budget; otherwise to the GPU, with no CPU
//! warm-up or re-probe during playback. One comp's sections can differ a lot (a long edit with
//! a light title section and a heavy one), so a CPU frame slower than the average counts in
//! full: the next frames go to the GPU (the side that keeps up) instead of a run of heavy CPU
//! frames. Slow GPU frames still count at most twice the average, so a stall of the machine
//! does not send heavy frames to the CPU.

use std::collections::HashMap;
use std::sync::Mutex;

use effectcraft_project::ItemId;

/// Which frames share timing history: one comp at one output scale on one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AutoKey {
    pub comp: ItemId,
    /// `RenderOpts::scale` bits.
    pub scale: u64,
    /// The viewer's display path (GPU frames stay on the device) rather than a readback.
    pub display: bool,
}

impl AutoKey {
    pub fn new(comp: ItemId, scale: f64, display: bool) -> AutoKey {
        AutoKey { comp, scale: scale.to_bits(), display }
    }
}

/// Timing history of one [`AutoKey`].
#[derive(Clone, Copy, Debug, Default)]
pub struct AutoStats {
    /// Averaged ms/frame (`None` until a counted sample).
    pub cpu_ms: Option<f64>,
    pub gpu_ms: Option<f64>,
    /// Frames measured on each side (the first is the uncounted warm-up).
    pub cpu_frames: u32,
    pub gpu_frames: u32,
    /// Frames since the side not chosen was last measured.
    pub since_probe: u32,
    /// Frames between re-measurements of the side not chosen (0: [`AutoPick::REPROBE`]).
    pub reprobe: u32,
    /// The GPU declined the last frame it was asked for (the CPU rendered it).
    pub declined: bool,
}

impl AutoStats {
    /// Both sides measured, and the GPU clearly faster ([`AutoPick::CLEAR`]).
    fn gpu_clearly_faster(&self) -> bool {
        !self.declined && matches!((self.gpu_ms, self.cpu_ms), (Some(g), Some(c)) if g <= c * AutoPick::CLEAR)
    }

    fn reprobe_interval(&self) -> u32 {
        if self.reprobe == 0 { AutoPick::REPROBE } else { self.reprobe }
    }
}

/// Per-comp CPU / GPU timing history for [`Backend::Auto`](crate::Backend::Auto) (held by the
/// accelerator, see [`Accelerator::auto_pick`](crate::Accelerator::auto_pick)).
#[derive(Default)]
pub struct AutoPick {
    stats: Mutex<HashMap<AutoKey, AutoStats>>,
}

impl AutoPick {
    /// Frames measured on each side before choosing (the first is not counted).
    pub const WARMUP: u32 = 3;
    /// Weight of a new sample in the running average (slower / faster than the average).
    pub const ALPHA: f64 = 0.3;
    pub const ALPHA_DOWN: f64 = 0.5;
    /// Re-measure the side not chosen after this many frames.
    pub const REPROBE: u32 = 48;
    /// The CPU wins only when clearly faster (ties and noise stay on the GPU).
    pub const MARGIN: f64 = 0.85;
    /// The GPU is clearly faster when its frames take at most this share of the CPU's: the
    /// CPU is then re-measured less often, and other resolutions of the comp skip its warm-up.
    pub const CLEAR: f64 = 0.5;
    /// The longest re-probe interval (the interval doubles on each probe that finds the GPU
    /// clearly faster).
    pub const MAX_REPROBE: u32 = 8 * Self::REPROBE;
    /// Keys remembered before the history is reset.
    const MAX_KEYS: usize = 1024;

    /// Render the frame for `key` on the GPU?
    pub fn choose(&self, key: AutoKey) -> bool {
        let Ok(mut m) = self.stats.lock() else { return true };
        if m.len() > Self::MAX_KEYS && !m.contains_key(&key) {
            m.clear();
        }
        if !m.contains_key(&key) {
            let seed = m.iter().find(|(k, s)| k.comp == key.comp && k.display == key.display && s.gpu_clearly_faster()).map(|_| AutoStats {
                // The CPU side counts as warmed up, unmeasured: the GPU is chosen until the
                // first re-probe measures the CPU at this resolution.
                cpu_frames: Self::WARMUP,
                ..AutoStats::default()
            });
            m.insert(key, seed.unwrap_or_default());
        }
        let Some(s) = m.get_mut(&key) else { return true };
        // Warm up both sides, alternating, GPU first.
        let gpu_warm = s.declined || s.gpu_frames >= Self::WARMUP;
        if !gpu_warm && s.gpu_frames <= s.cpu_frames {
            return true;
        }
        if s.cpu_frames < Self::WARMUP {
            return false;
        }
        let gpu_faster = match (s.gpu_ms, s.cpu_ms) {
            (Some(g), Some(c)) => c >= g * Self::MARGIN,
            (g, _) => g.is_some(),
        };
        // Periodically re-measure the slower side.
        if s.since_probe >= s.reprobe_interval() {
            s.since_probe = 0;
            return !gpu_faster;
        }
        s.since_probe += 1;
        gpu_faster
    }

    /// Render a frame for `key` that a playing preview needs within `budget_ms` on the GPU?
    /// The CPU only when it measured within the budget (then as [`Self::choose`]); else the GPU,
    /// unless it declined `key`. Takes no CPU warm-up or re-probe frames otherwise.
    pub fn choose_within(&self, key: AutoKey, budget_ms: f64) -> bool {
        let fits = {
            let Ok(m) = self.stats.lock() else { return true };
            match m.get(&key) {
                Some(s) => s.declined || s.cpu_ms.is_some_and(|c| c <= budget_ms),
                None => false,
            }
        };
        if fits { self.choose(key) } else { true }
    }

    /// A frame for `key` took `ms` on the GPU (`gpu`) or the CPU.
    ///
    /// The warm-up keeps the fastest of its counted frames; after that the average follows
    /// improvements quickly and slow-downs gradually. A single GPU frame counts at most twice
    /// the average (a stall of the machine does not move heavy frames to the CPU for a whole
    /// re-probe period); a slow CPU frame counts in full (see the module docs).
    pub fn record(&self, key: AutoKey, gpu: bool, ms: f64) {
        let Ok(mut m) = self.stats.lock() else { return };
        let s = m.entry(key).or_default();
        let (avg, frames) = if gpu { (&mut s.gpu_ms, &mut s.gpu_frames) } else { (&mut s.cpu_ms, &mut s.cpu_frames) };
        *frames = frames.saturating_add(1);
        if gpu {
            s.declined = false;
        }
        if *frames < 2 {
            return;
        }
        let probe = *frames > Self::WARMUP;
        *avg = Some(match *avg {
            None => ms,
            Some(a) if *frames <= Self::WARMUP => a.min(ms),
            Some(a) if ms < a => a + (ms - a) * Self::ALPHA_DOWN,
            Some(a) if !gpu && ms > 2.0 * a => ms,
            Some(a) => a + (ms.min(2.0 * a) - a) * Self::ALPHA,
        });
        // A CPU re-probe: back off while the GPU stays clearly faster, else the usual cadence.
        if !gpu && probe {
            s.reprobe = if s.gpu_clearly_faster() { s.reprobe_interval().saturating_mul(2).min(Self::MAX_REPROBE) } else { Self::REPROBE };
        }
    }

    /// The GPU did not handle a frame for `key` (it rendered on the CPU instead).
    pub fn declined(&self, key: AutoKey) {
        if let Ok(mut m) = self.stats.lock() {
            let s = m.entry(key).or_default();
            s.declined = true;
            s.gpu_ms = None;
            s.since_probe = 0;
            s.reprobe = AutoPick::REPROBE;
        }
    }

    /// The history of `key`.
    pub fn stats(&self, key: AutoKey) -> Option<AutoStats> {
        self.stats.lock().ok()?.get(&key).copied()
    }

    /// Forget all history (settings that change both paths' costs).
    pub fn reset(&self) {
        if let Ok(mut m) = self.stats.lock() {
            m.clear();
        }
    }
}
