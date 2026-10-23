//! The threads Cache Frames When Idle renders on: a small Rayon pool of its own.
//!
//! Idle frames used to render like any prefetch frame: up to half the frame threads at once, each
//! frame's parallel pixel loops (blurs, shadows, quantisation) on Rayon's global pool, so "half"
//! became every core at normal priority, and the whole machine lagged while the app sat idle. Now
//! one idle frame renders at a time, inside this pool ([`rayon::ThreadPool::install`]), so its
//! parallel loops stay on these few threads ([`idle_threads`]: a quarter of the cores, 1 to 4, or
//! Memory & CPU ▸ CPU Cores for Cache Frames When Idle). Each thread runs the hook the app set
//! with [`set_thread_start`] first: the desktop app lowers it to background quality of service
//! (macOS utility QoS), so the UI, playback and other apps run first. The viewer's frame and
//! playback keep the frame threads and the global pool, and go first in the queue.
//!
//! Nothing interactive ever waits for an idle frame, which renders slowly on purpose:
//! - the viewer or playback asking for the frame that is rendering as an idle frame calls that
//!   render off and renders the frame again at its own priority (`Frames::request_with`; the idle
//!   render is an orphan that frees only its idle slot when it ends);
//! - a layer an idle thread is rendering is rendered again by a viewer or playback frame that
//!   needs it, not waited for (`effectcraft_render::cache::set_background_thread`).
//!
//! Inside the pool an idle frame runs on a Rayon worker, so a layer it waits for (another frame
//! rendering it) is waited for at most the pool's short wait (`effectcraft_render::cache`); with
//! one idle frame at a time, no other idle frame is ever started inside its parallel loops.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

static THREAD_START: OnceLock<fn()> = OnceLock::new();

/// Run `hook` at the start of every idle-cache thread and RAM preview compression thread
/// ([`super::codec`]; the desktop app: lower its priority). The first call wins.
pub fn set_thread_start(hook: fn()) {
    let _ = THREAD_START.set(hook);
}

/// Run the [`set_thread_start`] hook on the calling thread, else lower it to background
/// priority the app's way (`effectcraft_render::concurrency::set_thread_priority_hooks`).
pub(crate) fn thread_start() {
    match THREAD_START.get() {
        Some(hook) => hook(),
        None => effectcraft_engine::render::concurrency::background_thread(),
    }
}

/// Threads of the idle pool on a machine with `cores` cores: `setting` (Memory & CPU ▸ CPU Cores
/// for Cache Frames When Idle) up to the cores there are, or with 0 (automatic) a quarter of
/// them, at least 1 and at most 4.
pub(crate) fn idle_threads(cores: usize, setting: u32) -> usize {
    let cores = cores.max(1);
    match setting {
        0 => (cores / 4).clamp(1, 4),
        n => (n as usize).min(cores),
    }
}

/// The idle pool, made when the first idle frame renders and again when its size changes.
#[derive(Default)]
pub(crate) struct IdlePool {
    /// (threads wanted, the pool if it was made).
    state: Mutex<(usize, Option<Arc<rayon::ThreadPool>>)>,
}

impl IdlePool {
    /// Use `threads` threads from the next idle frame on (the frame rendering keeps its pool).
    pub(crate) fn set_threads(&self, threads: usize) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if s.0 != threads {
            *s = (threads, None);
        }
    }

    /// The pool (`None` if the OS refused its threads: the frame then renders on the frame thread,
    /// as before).
    pub(crate) fn pool(&self) -> Option<Arc<rayon::ThreadPool>> {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if s.1.is_none() {
            let threads = if s.0 == 0 { idle_threads(std::thread::available_parallelism().map_or(4, |n| n.get()), 0) } else { s.0 };
            let built = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("ec-idle-{i}"))
                .start_handler(|_| {
                    // Nobody else waits for a layer these threads are rendering.
                    effectcraft_engine::render::cache::set_background_thread();
                    thread_start();
                })
                .build();
            match built {
                Ok(p) => s.1 = Some(Arc::new(p)),
                Err(e) => log::warn!("Cache Frames When Idle: no threads of its own ({e}); idle frames render on the frame threads"),
            }
        }
        s.1.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_pool_takes_a_quarter_of_the_cores_or_the_setting() {
        // Automatic: a quarter, at least one, at most four.
        assert_eq!(idle_threads(1, 0), 1);
        assert_eq!(idle_threads(4, 0), 1);
        assert_eq!(idle_threads(8, 0), 2);
        assert_eq!(idle_threads(10, 0), 2);
        assert_eq!(idle_threads(14, 0), 3);
        assert_eq!(idle_threads(16, 0), 4);
        assert_eq!(idle_threads(64, 0), 4);
        assert_eq!(idle_threads(0, 0), 1, "unknown core count");
        // The setting, up to the cores there are.
        assert_eq!(idle_threads(14, 6), 6);
        assert_eq!(idle_threads(8, 99), 8);
        assert_eq!(idle_threads(0, 3), 1);
    }

    /// An idle frame's parallel loops run on the idle pool's threads, not the global pool.
    #[test]
    fn parallel_loops_inside_the_pool_stay_on_its_threads() {
        use rayon::prelude::*;
        let p = IdlePool::default();
        p.set_threads(2);
        let pool = p.pool().expect("threads");
        assert_eq!(pool.current_num_threads(), 2);
        let names: std::collections::HashSet<String> =
            pool.install(|| (0..64).into_par_iter().map(|_| std::thread::current().name().unwrap_or_default().to_string()).collect());
        assert!(names.iter().all(|n| n.starts_with("ec-idle-")), "{names:?}");
        // Same size: the same pool; a new size: a new one.
        assert!(Arc::ptr_eq(&pool, &p.pool().expect("threads")));
        p.set_threads(1);
        assert_eq!(p.pool().expect("threads").current_num_threads(), 1);
    }
}
