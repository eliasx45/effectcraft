//! Bounded native render-service measurements for Auto playback resolution.
//! Cache hits and old playback/content/scale epochs never train this controller.
//!
//! A ladder Full → Half → Third → Quarter (as After Effects' resolutions): three slow renders out of the last four step down one
//! rung, so a slow cut is caught a few frames in; eight renders that would fit the budget
//! comfortably at the rung above step back up. A rung that is not measurably faster than
//! the one above it is abandoned and becomes the floor for the rest of the epoch.
//!
//! Frames render in parallel, so a render is judged by its share of the throughput, not
//! by its own length: its service time divided by the renders of the rung in flight when
//! it finished. Eight frames of 200 ms each in parallel deliver one frame every 25 ms,
//! which keeps up with 30 fps. Stepping up assumes the rung above fills the whole pool
//! (`Config::parallel`); a step up that is undone soon after falls back to the measured
//! parallelism for the rest of the epoch, so the ladder does not oscillate.
use std::collections::VecDeque;
use std::sync::Arc;

use crate::frames::FrameKey;

const WINDOW: usize = 32;
/// The lowest rung (0 Full, 1 Half, 2 Third, 3 Quarter).
pub(crate) const QUARTER: usize = 3;
// AutoPick warms three frames per compositor (CPU and GPU). Discard those renders when
// playback starts, so cold uploads and compositor exploration do not decide.
const WARMUP: usize = 6;
// After a rung change only the first cold renders at the new scale are discarded.
const STEP_WARMUP: usize = 3;
/// Step down when `SLOW_OF` of the last `DOWN` renders miss the deadline.
const DOWN: usize = 4;
const SLOW_OF: usize = 3;
/// Renders measured at a new lower rung before judging whether it is faster.
const VERIFY: usize = 4;
/// Step up when the median of the last `UP` renders, scaled by the rung above's pixel count,
/// fits within `UP_HEADROOM` of the budget.
const UP: usize = 8;
const UP_HEADROOM: f64 = 0.75;
/// A step down within this many renders of a step up marks the step up as optimistic.
const BOUNCE: usize = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    pub full: FrameKey,
    pub half: FrameKey,
    pub third: FrameKey,
    pub quarter: FrameKey,
    pub budget_ms: f64,
    pub gpu_context: usize,
    /// Frames the render pool runs at once (at least 1).
    pub parallel: usize,
}

impl Config {
    fn same(&self, other: &Self) -> bool {
        self.full.same_series(&other.full)
            && self.full.revision == other.full.revision
            && self.half.same_series(&other.half)
            && self.third.same_series(&other.third)
            && self.quarter.same_series(&other.quarter)
            && self.budget_ms == other.budget_ms
            && self.gpu_context == other.gpu_context
    }

    fn rung(&self, level: usize) -> FrameKey {
        match level {
            0 => self.full,
            1 => self.half,
            2 => self.third,
            _ => self.quarter,
        }
    }

    /// The next rung down from `level`: the first one smaller than it (the Adaptive Resolution
    /// Limit makes the lower rungs the same size).
    fn below(&self, level: usize) -> Option<usize> {
        let here = self.rung(level).scale;
        (level.saturating_add(1)..=QUARTER).find(|&l| self.rung(l).scale < here)
    }

    /// The next rung up from `level`: the nearest larger one.
    fn above(&self, level: usize) -> Option<usize> {
        let here = self.rung(level).scale;
        (0..level).rev().find(|&l| self.rung(l).scale > here)
    }

    /// The lowest usable rung (0: no smaller rung at all).
    fn bottom(&self) -> usize {
        let mut level = 0;
        while let Some(next) = self.below(level) {
            level = next;
        }
        level
    }
}

#[derive(Clone)]
pub(crate) struct Ticket(Arc<()>);

#[derive(Default)]
pub(crate) struct Budget {
    config: Option<Config>,
    epoch: Arc<()>,
    /// Per render: (its share of the throughput, its service time over the pool size).
    samples: VecDeque<(f64, f64)>,
    residency: Option<bool>,
    /// The rung in use: 0 Full, 1 Half, 2 Third, 3 Quarter.
    level: usize,
    /// Lowest rung allowed this epoch (a rung that was not faster raises it).
    floor: usize,
    /// Median at the rung above, while the current rung is being verified.
    trial_reference: Option<f64>,
    warmup: usize,
    trial_samples: usize,
    /// Renders since the last step up (`usize::MAX`: none this epoch).
    since_up: usize,
    /// A step up was undone soon after: step up only on measured throughput.
    bounced: bool,
}

fn median(samples: impl Iterator<Item = f64>) -> f64 {
    let mut sorted: Vec<_> = samples.collect();
    sorted.sort_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied().unwrap_or(0.0)
}

impl Budget {
    /// The rung to render at (0 Full, 1 Half, 2 Third, 3 Quarter). A new playback/content epoch, or
    /// no adaptive playback at all (idle, an explicit resolution), restores Full.
    pub fn configure(&mut self, config: Option<Config>) -> usize {
        let config = config.filter(|c| c.budget_ms.is_finite() && c.budget_ms > 0.0 && c.bottom() > 0);
        let unchanged = match (&self.config, &config) {
            (Some(a), Some(b)) => a.same(b),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            // The pool size is no reason to start over; follow it.
            if let (Some(a), Some(b)) = (&mut self.config, config) {
                a.parallel = b.parallel;
            }
        } else {
            self.config = config;
            self.since_up = usize::MAX;
            self.bounced = false;
            self.level = 0;
            self.floor = config.map_or(0, |c| c.bottom());
            self.trial_reference = None;
            self.trial_samples = 0;
            self.reset_samples(WARMUP);
        }
        self.level
    }

    fn reset_samples(&mut self, warmup: usize) {
        self.epoch = Arc::new(());
        self.samples.clear();
        self.residency = None;
        self.warmup = warmup;
    }

    fn step(&mut self, level: usize) {
        if level < self.level {
            self.since_up = 0;
        } else if level > self.level {
            self.bounced |= self.since_up < BOUNCE;
            self.since_up = usize::MAX;
        }
        self.level = level;
        self.trial_samples = 0;
        self.reset_samples(STEP_WARMUP);
    }

    pub fn ticket(&self, key: FrameKey) -> Option<Ticket> {
        let expected = self.config.as_ref()?.rung(self.level);
        (key.same_series(&expected) && key.revision == expected.revision).then(|| Ticket(self.epoch.clone()))
    }

    /// A render of this epoch finished after `ms` of service. The caller still holds one
    /// more clone of `ticket` (the job's own), so the tickets alive besides `self.epoch` and
    /// `ticket` are the renders of this rung in flight, this one included.
    pub fn record(&mut self, ticket: Ticket, ms: f64, resident: bool) {
        if !Arc::ptr_eq(&ticket.0, &self.epoch) || !ms.is_finite() || ms <= 0.0 {
            return;
        }
        let in_flight = Arc::strong_count(&self.epoch).saturating_sub(2).max(1);
        drop(ticket);
        self.record_parallel(ms, in_flight, resident);
    }

    fn record_parallel(&mut self, ms: f64, in_flight: usize, resident: bool) {
        let Some(config) = self.config else { return };
        self.since_up = self.since_up.saturating_add(1);
        let share = ms / in_flight.max(1) as f64;
        let pooled = ms / in_flight.max(config.parallel).max(1) as f64;
        if self.trial_reference.is_some() {
            self.trial_samples = self.trial_samples.saturating_add(1);
            if self.trial_samples > 64 {
                // An unstable trial never settled: return to the rung above and stay there.
                self.trial_reference = None;
                self.floor = config.above(self.level).unwrap_or(0);
                self.step(self.floor);
                return;
            }
        }
        if self.residency.is_some_and(|old| old != resident) {
            self.samples.clear();
            self.warmup = STEP_WARMUP;
        }
        self.residency = Some(resident);
        if self.warmup > 0 {
            self.warmup = self.warmup.saturating_sub(1);
            return;
        }
        if self.samples.len() >= WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back((share, pooled));
        let n = self.samples.len();
        let shares = |k: usize| self.samples.iter().skip(n.saturating_sub(k)).map(|s| s.0);
        if let Some(reference) = self.trial_reference {
            if n < VERIFY {
                return;
            }
            self.trial_reference = None;
            // Fewer pixels are not necessarily faster: Auto may choose different compositors
            // at each scale. Keep the lower rung only with a clear measured win.
            if median(self.samples.iter().map(|s| s.0)) >= reference * 0.9 {
                self.floor = config.above(self.level).unwrap_or(0);
                self.step(self.floor);
                return;
            }
        }
        let threshold = config.budget_ms * 1.1;
        if let Some(next) = config.below(self.level).filter(|&l| l <= self.floor)
            && n >= DOWN
            && shares(DOWN).filter(|ms| *ms > threshold).count() >= SLOW_OF
        {
            let reference = median(shares(DOWN));
            // Far over budget (a heavy stretch), go straight to the first rung predicted to fit
            // (cost grows with the pixel count), or the lowest allowed: one rung per four slow
            // frames took seconds at a frame a second.
            let here = config.rung(self.level).scale.max(1) as f64;
            let mut target = next;
            while reference * (config.rung(target).scale as f64 / here).powi(2) > config.budget_ms {
                match config.below(target).filter(|&l| l <= self.floor) {
                    Some(l) => target = l,
                    None => break,
                }
            }
            self.step(target);
            self.trial_reference = Some(reference);
            return;
        }
        if let Some(up) = config.above(self.level)
            && n >= UP
        {
            // Cost grows with the pixel count: predict the rung above from this one.
            let above = config.rung(up).scale as f64;
            let here = config.rung(self.level).scale.max(1) as f64;
            let recent = self.samples.iter().skip(n.saturating_sub(UP));
            let cost = if self.bounced { median(recent.map(|s| s.0)) } else { median(recent.map(|s| s.1)) };
            let predicted = cost * (above / here).powi(2);
            if predicted <= config.budget_ms * UP_HEADROOM {
                self.step(up);
            }
        }
    }
}

/// The viewer's draft badge: the resolution of the frame on screen (`shown`, render scale × 1000)
/// against the one the viewer asks for at full quality (`full`). `None`: full quality.
pub(crate) fn draft_label(full: u32, shown: u32) -> Option<&'static str> {
    if full == 0 || u64::from(shown) * 100 >= u64::from(full) * 98 {
        return None;
    }
    let ratio = f64::from(shown) / f64::from(full);
    Some(if ratio >= 0.6 {
        "Draft"
    } else if ratio >= 0.45 {
        "Half"
    } else if ratio >= 0.3 {
        "Third"
    } else if ratio >= 0.2 {
        "Quarter"
    } else {
        "Lower"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        let full = FrameKey { revision: 1, content: 1, comp: 1, frame: 0, scale: 1000, view: 0, opts: 1 };
        Config {
            full,
            half: FrameKey { scale: 500, opts: 2, ..full },
            third: FrameKey { scale: 333, opts: 4, ..full },
            quarter: FrameKey { scale: 250, opts: 3, ..full },
            budget_ms: 1000.0 / 30.0,
            gpu_context: 0,
            parallel: 1,
        }
    }
    /// Record a render at the rung in use (`false` when no ticket is issued for it).
    fn at(b: &mut Budget, key: FrameKey, ms: f64, gpu: bool) -> bool {
        let Some(t) = b.ticket(key) else { return false };
        b.record(t, ms, gpu);
        true
    }
    fn sample(b: &mut Budget, c: Config, ms: f64, gpu: bool) {
        assert!(at(b, c.full, ms, gpu));
    }
    fn warm(b: &mut Budget, c: Config) {
        for _ in 0..WARMUP {
            sample(b, c, 10.0, false);
        }
    }

    #[test]
    fn slow_stretch_reduces_within_four_renders() {
        let c = config();
        let mut b = Budget::default();
        assert_eq!(b.configure(Some(c)), 0);
        warm(&mut b, c);
        for _ in 0..8 {
            sample(&mut b, c, 10.0, false);
        }
        for _ in 0..SLOW_OF - 1 {
            sample(&mut b, c, 50.0, false);
        }
        assert_eq!(b.configure(Some(c)), 0);
        sample(&mut b, c, 50.0, false);
        assert_eq!(b.configure(Some(c)), 1);
        assert!(b.ticket(c.full).is_none());
        assert!(b.ticket(c.half).is_some());
        // Idle (or any explicit choice, which configures nothing) restores Full.
        assert_eq!(b.configure(None), 0);
        assert_eq!(b.configure(Some(c)), 0);
    }

    #[test]
    fn ladder_steps_to_quarter_while_half_is_slow_and_back_up_when_comfortable() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 100.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
        // Half is faster, but still over budget: on to Third, then Quarter.
        for _ in 0..STEP_WARMUP + DOWN {
            assert!(at(&mut b, c.half, 60.0, false));
        }
        assert_eq!(b.configure(Some(c)), 2);
        for _ in 0..STEP_WARMUP + DOWN {
            assert!(at(&mut b, c.third, 40.0, false));
        }
        assert_eq!(b.configure(Some(c)), QUARTER);
        // The cut ends: Quarter renders that would fit above step up one rung at a time.
        for _ in 0..STEP_WARMUP + UP {
            assert!(at(&mut b, c.quarter, 3.0, false));
        }
        assert_eq!(b.configure(Some(c)), 2);
        for _ in 0..STEP_WARMUP + UP {
            assert!(at(&mut b, c.third, 3.0, false));
        }
        assert_eq!(b.configure(Some(c)), 1);
        for _ in 0..STEP_WARMUP + UP {
            assert!(at(&mut b, c.half, 5.0, false));
        }
        assert_eq!(b.configure(Some(c)), 0);
        // Renders that would only just fit above stay down (no oscillation).
        for _ in 0..STEP_WARMUP + DOWN {
            sample(&mut b, c, 100.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
        for _ in 0..STEP_WARMUP + 100 {
            assert!(at(&mut b, c.half, 8.0, false));
        }
        assert_eq!(b.configure(Some(c)), 1);
    }

    #[test]
    fn quarter_is_skipped_when_it_is_not_smaller_than_half() {
        let mut c = config();
        c.third = c.half;
        c.quarter = c.half;
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 100.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
        for _ in 0..100 {
            assert!(at(&mut b, c.half, 60.0, false));
        }
        assert_eq!(b.configure(Some(c)), 1);
    }

    /// The Adaptive Resolution Limit at 1/4 makes Third and Quarter the same size: Half steps
    /// straight to the one smaller rung, and back up to Half.
    #[test]
    fn rungs_of_the_same_size_are_one_rung() {
        let mut c = config();
        c.third = FrameKey { scale: 250, ..c.third };
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 100.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
        for _ in 0..STEP_WARMUP + DOWN {
            assert!(at(&mut b, c.half, 60.0, false));
        }
        assert_eq!(b.configure(Some(c)), 2, "Third (the limit's size) is the next rung");
        // Faster than Half (kept), still over budget, but Quarter is no smaller: no step.
        for _ in 0..100 {
            assert!(at(&mut b, c.third, 40.0, false));
        }
        assert_eq!(b.configure(Some(c)), 2, "Quarter is no smaller: no step");
        // Light again: back up to Half (once the window's median is light, before all of `UP`).
        for _ in 0..UP {
            at(&mut b, c.third, 2.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
    }

    /// A stretch far over budget goes straight to the rung predicted to fit, and steps back up
    /// one rung at a time.
    #[test]
    fn far_over_budget_skips_straight_to_the_rung_that_fits() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 200.0, false);
        }
        assert_eq!(b.configure(Some(c)), 2, "200 ms × (1/3)² = 22 ms fits 33 ms: Third");
        b.configure(None);
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 5000.0, false);
        }
        assert_eq!(b.configure(Some(c)), QUARTER, "nothing fits: the lowest rung");
        for _ in 0..STEP_WARMUP + UP {
            assert!(at(&mut b, c.quarter, 3.0, false));
        }
        assert_eq!(b.configure(Some(c)), 2);
    }

    #[test]
    fn draft_labels_name_the_resolution_on_screen() {
        assert_eq!(draft_label(1000, 1000), None);
        assert_eq!(draft_label(1000, 995), None);
        assert_eq!(draft_label(1000, 500), Some("Half"));
        assert_eq!(draft_label(2000, 667), Some("Third"));
        assert_eq!(draft_label(1000, 250), Some("Quarter"));
        assert_eq!(draft_label(1000, 125), Some("Lower"));
        assert_eq!(draft_label(0, 0), None);
    }

    #[test]
    fn explicit_resolution_and_invalid_budgets_never_adapt() {
        let c = config();
        let mut b = Budget::default();
        // An explicit resolution, Fast Previews off or export configure nothing.
        assert_eq!(b.configure(None), 0);
        assert!(b.ticket(c.full).is_none());
        for budget_ms in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(b.configure(Some(Config { budget_ms, ..c })), 0);
            assert!(b.ticket(c.full).is_none());
        }
        // A Half no smaller than Full (the adaptive limit) is no ladder either.
        let flat = Config { half: c.full, third: c.full, quarter: c.full, ..c };
        assert_eq!(b.configure(Some(flat)), 0);
        assert!(b.ticket(c.full).is_none());
    }

    #[test]
    fn one_hitch_invalid_samples_and_cache_only_polls_do_not_reduce() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        for ms in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            sample(&mut b, c, ms, false);
        }
        for _ in 0..31 {
            sample(&mut b, c, 10.0, false);
        }
        sample(&mut b, c, 100.0, false);
        for _ in 0..100 {
            assert_eq!(b.configure(Some(c)), 0);
        }
        sample(&mut b, c, 100.0, false);
        assert_eq!(b.configure(Some(c)), 0);
    }

    #[test]
    fn stale_epochs_series_revisions_and_backend_history_are_rejected() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        let old = b.ticket(c.full).unwrap();
        b.configure(None);
        b.configure(Some(c));
        b.record(old, 1000.0, false);
        assert!(b.samples.is_empty());
        assert!(b.ticket(FrameKey { revision: 2, ..c.full }).is_none());
        assert!(b.ticket(FrameKey { content: 2, ..c.full }).is_none());
        warm(&mut b, c);
        for _ in 0..DOWN - 1 {
            sample(&mut b, c, 100.0, false);
        }
        sample(&mut b, c, 10.0, true);
        assert!(b.samples.is_empty());
        assert_eq!(b.configure(Some(c)), 0);
        let changed = Config { full: FrameKey { content: 2, ..c.full }, ..c };
        assert_eq!(b.configure(Some(changed)), 0);
        assert!(b.samples.is_empty());
    }

    #[test]
    fn slower_half_trial_restores_full_without_repeated_trials() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 50.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
        for _ in 0..STEP_WARMUP + VERIFY {
            assert!(at(&mut b, c.half, 80.0, false));
        }
        assert_eq!(b.configure(Some(c)), 0);
        for _ in 0..100 {
            sample(&mut b, c, 1000.0, false);
        }
        assert_eq!(b.configure(Some(c)), 0);
        b.configure(None);
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 50.0, false);
        }
        assert_eq!(b.configure(Some(c)), 1);
    }

    #[test]
    fn cold_compositor_trials_do_not_choose_resolution() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        for _ in 0..WARMUP {
            sample(&mut b, c, 1000.0, false);
        }
        for _ in 0..UP {
            sample(&mut b, c, 10.0, false);
        }
        assert_eq!(b.configure(Some(c)), 0);
    }

    #[test]
    fn sporadic_missed_deadlines_do_not_reduce() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for i in 0..64 {
            sample(&mut b, c, if i % 2 == 0 { 60.0 } else { 20.0 }, false);
        }
        assert_eq!(b.configure(Some(c)), 0);
    }

    /// Keep `n` renders of the rung in use in flight and finish them one at a time, as the
    /// pool does in steady state (the job keeps its own ticket while it is recorded).
    fn parallel(b: &mut Budget, key: FrameKey, n: usize, ms: f64, renders: usize) {
        let mut flight: VecDeque<Ticket> = (0..n).filter_map(|_| b.ticket(key)).collect();
        for _ in 0..renders {
            let Some(t) = flight.pop_front() else { return };
            b.record(t.clone(), ms, false);
            drop(t);
            flight.extend(b.ticket(key));
        }
    }

    /// A5: eight frames of 200 ms each in parallel deliver a frame every 25 ms, which keeps
    /// up with 30 fps: no step down. The same renders one at a time, or eight of 400 ms, miss.
    #[test]
    fn parallel_renders_that_keep_up_stay_at_full() {
        let c = Config { parallel: 8, ..config() };
        let mut b = Budget::default();
        b.configure(Some(c));
        parallel(&mut b, c.full, 8, 200.0, WARMUP + 200);
        assert_eq!(b.configure(Some(c)), 0);
        parallel(&mut b, c.full, 8, 400.0, DOWN);
        assert_eq!(b.configure(Some(c)), 1, "genuinely slow throughput still steps down");
        b.configure(None);
        b.configure(Some(c));
        parallel(&mut b, c.full, 1, 200.0, WARMUP + DOWN);
        assert_eq!(b.configure(Some(c)), 2, "one frame at a time is slow: 200 ms × ¼ still misses, Third fits");
    }

    /// A5: Half delivering on demand (few renders in flight) steps back up when the pool
    /// would carry Full; a step up that is undone right away is not tried again on that
    /// estimate.
    #[test]
    fn half_steps_back_up_when_the_pool_carries_full_and_does_not_oscillate() {
        let c = Config { parallel: 8, ..config() };
        let mut b = Budget::default();
        b.configure(Some(c));
        parallel(&mut b, c.full, 8, 600.0, WARMUP + DOWN);
        assert_eq!(b.configure(Some(c)), 1);
        // The cut ends: Half renders 30 ms each, one at a time (on demand).
        parallel(&mut b, c.half, 1, 30.0, STEP_WARMUP + UP);
        assert_eq!(b.configure(Some(c)), 0, "Full fits the pool: 4 × 30 / 8 = 15 ms ≤ 25 ms");
        // But Full turns out slow again: down, and the optimistic estimate is not used again.
        parallel(&mut b, c.full, 8, 400.0, STEP_WARMUP + DOWN);
        assert_eq!(b.configure(Some(c)), 1);
        parallel(&mut b, c.half, 1, 30.0, STEP_WARMUP + 200);
        assert_eq!(b.configure(Some(c)), 1);
        // A genuinely light stretch still steps up on measured throughput.
        parallel(&mut b, c.half, 1, 5.0, UP);
        assert_eq!(b.configure(Some(c)), 0);
    }

    #[test]
    fn unstable_trial_is_bounded_and_old_half_completion_cannot_retrain_full() {
        let c = config();
        let mut b = Budget::default();
        b.configure(Some(c));
        warm(&mut b, c);
        for _ in 0..DOWN {
            sample(&mut b, c, 50.0, false);
        }
        let late = b.ticket(c.half).unwrap();
        for i in 0..65 {
            assert!(at(&mut b, c.half, 80.0, i % 2 == 0));
        }
        assert_eq!(b.configure(Some(c)), 0);
        b.record(late, 1.0, false);
        assert!(b.samples.is_empty());
    }
}
