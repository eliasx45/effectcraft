//! CR1: how many viewer frames composite on the GPU at once, from the video memory they turn out
//! to take.
//!
//! A frame's working set on the GPU (canvases, precomps, effect temporaries) can be ten times the
//! four frame-sized buffers [`super::max_running`] estimates: sixteen 1080p frames at once filled
//! a 20 GB card. So on a discrete GPU:
//! - a new frame starts while the card has room for it, keeping a tenth of it free: its video
//!   memory as last asked of the driver, about every second, so another program taking memory
//!   leaves fewer frames ([`effectcraft_gpu::vram`]; 2 GiB in all when it is unknown);
//! - where wgpu reports the device's allocations (Vulkan, Direct3D 12), each finished frame
//!   measures what the frames rendering took per frame, and the cap follows that;
//! - until a frame has been measured, one is assumed to take a sixteenth of the card, and the
//!   first frames start two at a time, one more for each that finished (a slow start), so the
//!   measurement comes before many frames run at once;
//! - each time the GPU runs out of memory, at most half as many frames as were rendering then
//!   run from then on, also on a compositor that replaces a failed one.
//!
//! Integrated GPUs (unified memory) keep the RAM-based caps.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use effectcraft_gpu::Gpu;

/// Frame working budget on a discrete GPU whose video memory is unknown.
pub(crate) const UNKNOWN_VRAM_BUDGET: u64 = 2 << 30;

/// Measure the device at most this often (it walks every allocation).
const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

/// The adaptive GPU frame cap ([`Gate::limit`]), shared by the queue and the frame workers.
pub(crate) struct Gate {
    /// GPU frames rendering now, and the most at once since the compositor started.
    rendering: Arc<AtomicUsize>,
    peak: AtomicUsize,
    /// At most this many after running out of memory (`usize::MAX`: no limit yet).
    ceiling: AtomicUsize,
    /// GPU frames finished on this compositor (the slow start).
    finished: AtomicUsize,
    /// Bytes a GPU frame takes on the device, as measured (0: not yet).
    per_frame: AtomicU64,
    /// Device bytes with no GPU frame rendering (`u64::MAX`: not measured), and at the last
    /// measurement.
    baseline: AtomicU64,
    last_bytes: AtomicU64,
    /// The device reports its allocations: 0 not known yet, 1 yes, 2 no.
    measurable: std::sync::atomic::AtomicU8,
    last_sample: Mutex<Option<web_time::Instant>>,
    /// The driver's budget was logged (once per compositor).
    budget_logged: std::sync::atomic::AtomicBool,
}

impl Default for Gate {
    fn default() -> Self {
        Gate {
            rendering: Arc::default(),
            peak: AtomicUsize::new(0),
            ceiling: AtomicUsize::new(usize::MAX),
            finished: AtomicUsize::new(0),
            per_frame: AtomicU64::new(0),
            baseline: AtomicU64::new(u64::MAX),
            last_bytes: AtomicU64::new(0),
            measurable: std::sync::atomic::AtomicU8::new(0),
            last_sample: Mutex::new(None),
            budget_logged: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// A frame rendering with the GPU attached ([`Gate::enter`]: composited on it, or composited on
/// the CPU with its effects and uploads on the GPU); counted out when dropped.
pub(crate) struct Entered<'a> {
    gate: &'a Gate,
    gpu: Gpu,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.gate.finished(&self.gpu);
        // Paired with `enter`'s increment.
        if self.gate.rendering.fetch_sub(1, Ordering::AcqRel) == 1 {
            // Nothing rendering: what the device holds now is the baseline.
            if let Some(bytes) = self.gate.sample(&self.gpu, true) {
                self.gate.baseline.store(bytes, Ordering::Release);
            }
        }
    }
}

const GIB: u64 = 1 << 30;

/// LVR1: the bytes a GPU frame is assumed to take before one is measured on a card of `total`
/// bytes: a sixteenth of it, at least 1 GiB (a quarter of a card under 4 GiB). Heavy 3D comps took
/// 500-800 MB a frame; with a sixteenth (512 MB) an 8 GB card started 13 frames at once and filled
/// its memory in 4 s (RTX 4060 Laptop, 6 Nov 2026). From 16 GiB on, a sixteenth as before.
fn assumed_frame_bytes(total: u64) -> u64 {
    (total / 16).max(GIB.min(total / 4))
}

/// LVR1: video memory kept free on a card of `total` bytes: a tenth, at least 2 GiB (a quarter of
/// a card under 8 GiB). The driver's budget is less than the card (7.2 of 8 GB on that laptop) and
/// the window and the desktop need their part: a tenth of 8 GB left the compositor allowed past the
/// point where its allocations fail. From 20 GiB on, a tenth as before.
fn kept_free(total: u64) -> u64 {
    (total / 10).max((2 * GIB).min(total / 4))
}

/// LVR1: GPU frames at once on a card of `total` bytes, at most: one per 2 GiB, at least two
/// (4 on 8 GB, 12 on 24 GB). The live room is asked of the driver about once a second, slower than
/// frames start: this bounds what the first second can take.
fn frames_cap(total: u64) -> usize {
    usize::try_from(total / (2 * GIB)).unwrap_or(usize::MAX).max(2)
}

impl Gate {
    /// GPU frames that may render at once on `g` when one takes at least `frame` bytes (the
    /// static estimate), with `threads` frame threads. `None` on an integrated GPU (or another
    /// without video memory of its own): the caller keeps its RAM-based cap, under
    /// [`Gate::ceiling`].
    pub(crate) fn limit(&self, g: &Gpu, frame: f64, threads: usize) -> Option<usize> {
        let ctx = g.context();
        let vram = ctx.video_memory();
        if vram.is_none() && !ctx.discrete() {
            return None;
        }
        let learned = self.per_frame.load(Ordering::Acquire);
        // Until a frame has been measured, assume one takes a sixteenth of the card (heavy comps
        // took about 1 GB a frame on a 24 GB card): the first frames never fill it at once.
        let assumed = if learned == 0 { vram.map_or(0, |m| assumed_frame_bytes(m.total)) } else { learned };
        let per = (assumed as f64).max(frame).max(1.0);
        let rendering = self.rendering.load(Ordering::Acquire);
        // The frames rendering may still grow to a whole frame each: that room is kept for them.
        let reserve = || {
            let grown = match self.baseline.load(Ordering::Acquire) {
                u64::MAX => 0,
                b => self.last_bytes.load(Ordering::Acquire).saturating_sub(b),
            };
            ((rendering as f64 * per) as u64).saturating_sub(grown)
        };
        // LVR1: where the driver reports this process's use and budget (Vulkan), frames start
        // only below the soft ceiling under wgpu's threshold (`HeapBudget::soft_ceiling`): the
        // exact numbers wgpu checks, now, instead of the card's free memory a second ago.
        if let Some(b) = ctx.heap_budget() {
            if !self.budget_logged.swap(true, Ordering::AcqRel) {
                log::info!(
                    "GPU frames: video memory budget {} MiB, {} MiB used, new frames only below {} MiB",
                    b.budget >> 20,
                    b.usage >> 20,
                    b.soft_ceiling() >> 20
                );
            }
            let room = b.room().saturating_sub(reserve());
            let cap = frames_cap(vram.map_or(b.budget, |m| m.total));
            let mut n = rendering.saturating_add((room as f64 / per) as usize).min(cap).clamp(1, threads.max(1));
            if self.measurable.load(Ordering::Acquire) != 2 {
                n = n.min(self.finished.load(Ordering::Acquire).saturating_add(2));
            }
            return Some(n.min(self.ceiling()));
        }
        let mut n = match ctx.live_video_memory() {
            // The frames rendering now, and as many more as the room left on the card holds,
            // keeping a tenth of it free (other programs, the driver): another program that
            // takes memory (another EffectCraft on the same card) leaves fewer frames. The
            // frames rendering may still grow to a whole frame each: that room is kept for them
            // (a heavy stretch of the comp took 9 GB in a second, 8 frames at once).
            Some(m) => {
                let room = m.free.saturating_sub(kept_free(m.total)).saturating_sub(reserve());
                rendering.saturating_add((room as f64 / per) as usize).min(frames_cap(m.total))
            }
            None => ((UNKNOWN_VRAM_BUDGET as f64 / per) as usize).min(vram.map_or(usize::MAX, |m| frames_cap(m.total))),
        }
        .clamp(1, threads.max(1));
        if self.measurable.load(Ordering::Acquire) != 2 {
            // One more frame at once for each one finished, from two.
            n = n.min(self.finished.load(Ordering::Acquire).saturating_add(2));
        }
        Some(n.min(self.ceiling()))
    }

    /// At most this many GPU frames at once after running out of memory (`usize::MAX`: no limit).
    pub(crate) fn ceiling(&self) -> usize {
        self.ceiling.load(Ordering::Acquire).max(1)
    }

    /// A frame starts rendering with `g` attached.
    pub(crate) fn enter(&self, g: &Gpu) -> Entered<'_> {
        let rendering = self.rendering.load(Ordering::Acquire);
        if self.baseline.load(Ordering::Acquire) == u64::MAX
            && rendering == 0
            && let Some(bytes) = self.sample(g, true)
        {
            self.baseline.store(bytes, Ordering::Release);
        } else if rendering > 0 {
            // What the frames rendering hold so far, before they finish.
            self.measure(g, rendering);
        }
        let n = self.rendering.fetch_add(1, Ordering::AcqRel).saturating_add(1);
        self.peak.fetch_max(n, Ordering::AcqRel);
        Entered { gate: self, gpu: g.clone() }
    }

    /// A frame rendered with `g` attached is done (it is still counted as rendering).
    fn finished(&self, g: &Gpu) {
        self.finished.fetch_add(1, Ordering::AcqRel);
        self.measure(g, self.rendering.load(Ordering::Acquire).max(1));
    }

    /// What the `n` frames rendering take on the device each, measured now (rate limited).
    fn measure(&self, g: &Gpu, n: usize) {
        let (Some(bytes), baseline) = (self.sample(g, false), self.baseline.load(Ordering::Acquire)) else { return };
        if baseline == u64::MAX {
            return;
        }
        let per = bytes.saturating_sub(baseline) / n.max(1) as u64;
        // The largest recent measurement, forgotten slowly (an eighth of the difference each
        // time): a cap that follows a heavier stretch of the comp at once, and a lighter one later.
        let old = self.per_frame.load(Ordering::Acquire);
        self.per_frame.store(if per >= old { per } else { old - (old - per) / 8 }, Ordering::Release);
    }

    /// A GPU frame ran out of memory with the frames rendering now: at most half as many from
    /// now on.
    pub(crate) fn out_of_memory(&self) {
        self.lower(self.rendering.load(Ordering::Acquire));
    }

    /// The compositor failed (retired): its replacement runs at most half as many GPU frames as
    /// ran at once on it.
    pub(crate) fn retired(&self) {
        self.lower(self.peak.load(Ordering::Acquire));
    }

    fn lower(&self, running: usize) {
        if running == 0 {
            // Nothing rendered on it: no measure of what ran out.
            return;
        }
        let half = (running / 2).max(1);
        let before = self.ceiling.fetch_min(half, Ordering::AcqRel);
        if half < before {
            log::warn!("the GPU ran out of memory with {running} frames rendering on it: at most {half} at once from now on");
        }
    }

    /// A new compositor (a replacement device): measure it from scratch, keeping the ceiling.
    pub(crate) fn revived(&self) {
        self.finished.store(0, Ordering::Release);
        self.per_frame.store(0, Ordering::Release);
        self.baseline.store(u64::MAX, Ordering::Release);
        self.peak.store(self.rendering.load(Ordering::Acquire), Ordering::Release);
        self.measurable.store(0, Ordering::Release);
        self.budget_logged.store(false, Ordering::Release);
    }

    /// Device bytes now, at most every [`SAMPLE_EVERY`] unless `always` (`None` between, or
    /// when the device doesn't report them).
    fn sample(&self, g: &Gpu, always: bool) -> Option<u64> {
        if self.measurable.load(Ordering::Acquire) == 2 {
            return None;
        }
        {
            let mut last = self.last_sample.lock().unwrap_or_else(PoisonError::into_inner);
            let now = web_time::Instant::now();
            if !always && last.is_some_and(|t| now.duration_since(t) < SAMPLE_EVERY) && self.measurable.load(Ordering::Acquire) == 1 {
                return None;
            }
            *last = Some(now);
        }
        let bytes = g.context().device_bytes();
        self.measurable.store(if bytes.is_some() { 1 } else { 2 }, Ordering::Release);
        if let Some(b) = bytes {
            self.last_bytes.store(b, Ordering::Release);
        }
        bytes
    }

    /// A gate past its slow start (tests of the steady-state caps).
    #[cfg(test)]
    pub(crate) fn warmed() -> Self {
        let g = Gate::default();
        g.finished.store(1 << 20, Ordering::Release);
        g
    }

    /// For `ui.inspect` / tests: (ceiling, learned bytes per frame, frames rendering).
    pub(crate) fn state(&self) -> (Option<usize>, u64, usize) {
        let c = self.ceiling.load(Ordering::Acquire);
        ((c != usize::MAX).then_some(c), self.per_frame.load(Ordering::Acquire), self.rendering.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LVR1: small cards assume heavier frames, keep more free and run fewer frames at once;
    /// big cards keep their caps (Apple Silicon never gets here: `limit` is `None` there).
    #[test]
    fn small_cards_start_fewer_frames_and_keep_more_free() {
        let gb = |n: u64| n * GIB;
        assert_eq!(assumed_frame_bytes(gb(8)), GIB, "8 GB: 1 GiB a frame, not 512 MiB");
        assert_eq!(assumed_frame_bytes(gb(24)), gb(24) / 16, "24 GB: unchanged");
        assert_eq!(assumed_frame_bytes(gb(2)), gb(2) / 4);
        assert_eq!(kept_free(gb(8)), 2 * GIB);
        assert_eq!(kept_free(gb(24)), gb(24) / 10, "24 GB: unchanged");
        assert_eq!(kept_free(gb(4)), GIB);
        assert_eq!(frames_cap(gb(8)), 4);
        assert_eq!(frames_cap(gb(24)), 12);
        assert_eq!(frames_cap(gb(2)), 2);
        assert_eq!(frames_cap(0), 2);
        for total in [0, 1, gb(1), gb(6), gb(8), gb(12), gb(16), gb(24), gb(80), u64::MAX] {
            assert!(kept_free(total) <= total && assumed_frame_bytes(total) <= total.max(1), "{total}");
        }
    }

    #[test]
    fn each_out_of_memory_halves_the_frames_rendering_down_to_one() {
        let g = Gate::default();
        assert_eq!(g.ceiling(), usize::MAX);
        g.rendering.store(16, Ordering::Release);
        g.out_of_memory();
        assert_eq!(g.ceiling(), 8);
        g.rendering.store(12, Ordering::Release);
        g.out_of_memory();
        assert_eq!(g.ceiling(), 6, "half of those rendering, never above the ceiling");
        g.rendering.store(30, Ordering::Release);
        g.out_of_memory();
        assert_eq!(g.ceiling(), 6, "a ceiling never rises");
        for _ in 0..10 {
            g.rendering.store(g.ceiling(), Ordering::Release);
            g.out_of_memory();
        }
        assert_eq!(g.ceiling(), 1);
    }

    #[test]
    fn a_retired_compositor_halves_the_most_that_ran_and_its_replacement_keeps_it() {
        let g = Gate::default();
        g.peak.store(16, Ordering::Release);
        g.finished.store(40, Ordering::Release);
        g.per_frame.store(1 << 30, Ordering::Release);
        g.retired();
        g.revived();
        assert_eq!(g.ceiling(), 8);
        assert_eq!((g.finished.load(Ordering::Acquire), g.per_frame.load(Ordering::Acquire)), (0, 0), "the new device is measured anew");
    }
}
