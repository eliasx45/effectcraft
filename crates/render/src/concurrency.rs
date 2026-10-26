//! How many frames render at once (CR1): the viewer's frame pool and an export's batches.
//!
//! By default one frame per core, between 2 and 16. `EFFECTCRAFT_FRAME_THREADS` (or the CLI's
//! `--frame-threads N`, [`set_frame_threads`]) caps it: on a machine with many cores and one GPU,
//! sixteen GPU frames at once can exhaust video memory.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The environment variable that caps [`frame_threads`].
pub const FRAME_THREADS_ENV: &str = "EFFECTCRAFT_FRAME_THREADS";

/// Most frame threads a cap may ask for.
const MAX_FRAME_THREADS: usize = 256;

/// Set by [`set_frame_threads`] (0: not set).
static OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Cap [`frame_threads`] at `n` for this process (the CLI's `--frame-threads`), over
/// [`FRAME_THREADS_ENV`]. Frame pools made earlier keep their size.
pub fn set_frame_threads(n: usize) {
    OVERRIDE.store(n.min(MAX_FRAME_THREADS), Ordering::Relaxed);
}

/// Frames that may render at once on a machine with `parallelism` cores: `parallelism` between 2
/// and 16, capped by [`set_frame_threads`] or [`FRAME_THREADS_ENV`] (at least 1).
pub fn frame_threads(parallelism: usize) -> usize {
    let default = parallelism.clamp(2, 16);
    match OVERRIDE.load(Ordering::Relaxed) {
        0 => cap(default, std::env::var(FRAME_THREADS_ENV).ok().as_deref()),
        n => default.min(n).max(1),
    }
}

/// `default` capped by an [`FRAME_THREADS_ENV`] value: a whole number of at least 1, else (with a
/// warning) no cap.
fn cap(default: usize, var: Option<&str>) -> usize {
    let Some(v) = var else { return default };
    match v.trim().parse::<usize>() {
        Ok(n) if n >= 1 => default.min(n.min(MAX_FRAME_THREADS)),
        _ => {
            log::warn!("{FRAME_THREADS_ENV}={v:?} is not a whole number of frames of at least 1: using {default}");
            default
        }
    }
}

/// Cores the desktop app leaves to the rest of the machine (MEM3): 2 from 8 cores, 1 from 4,
/// none below. With every core busy at normal priority during playback, other apps (the window
/// server, a screen recorder, dictation) stopped responding on a 10-core Mac.
pub fn spare_cores(parallelism: usize) -> usize {
    match parallelism {
        8.. => 2,
        4.. => 1,
        _ => 0,
    }
}

/// Threads the desktop app's own pools take on a machine with `parallelism` cores: all but the
/// [`spare_cores`], at least 1.
pub fn interactive_threads(parallelism: usize) -> usize {
    parallelism.saturating_sub(spare_cores(parallelism)).max(1)
}

/// Make Rayon's global pool [`interactive_threads`] wide (the desktop app, before anything runs
/// on it). Whether it was built here (`false`: something built it first, it keeps its size).
#[cfg(not(target_arch = "wasm32"))]
pub fn init_global_pool() -> bool {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    rayon::ThreadPoolBuilder::new().num_threads(interactive_threads(cores)).build_global().is_ok()
}

/// How the app changes the calling thread's priority ([`set_thread_priority_hooks`]).
#[derive(Clone, Copy)]
struct Hooks {
    background: fn(),
    foreground: fn(),
}

static HOOKS: std::sync::OnceLock<Hooks> = std::sync::OnceLock::new();

/// Set how a thread lowers itself for background work nobody waits for (prefetch, read-ahead,
/// Cache Frames When Idle, RAM preview compression) and raises itself again for work the viewer
/// or playback waits for. The desktop app: macOS utility and user-initiated quality of service
/// (`effectcraft_platform`). The first call wins; without one these do nothing.
pub fn set_thread_priority_hooks(background: fn(), foreground: fn()) {
    let _ = HOOKS.set(Hooks { background, foreground });
}

/// Lower the calling thread to background priority (see [`set_thread_priority_hooks`]).
pub fn background_thread() {
    if let Some(h) = HOOKS.get() {
        (h.background)();
    }
}

thread_local! {
    /// The priority [`thread_priority`] last gave this thread (`None`: never set).
    static FOREGROUND: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Give the calling thread foreground or background priority for its next piece of work (a
/// frame worker between jobs). Changes it only when it differs from the last call's.
pub fn thread_priority(foreground: bool) {
    let Some(h) = HOOKS.get() else { return };
    if FOREGROUND.with(|f| f.replace(Some(foreground))) != Some(foreground) {
        if foreground { (h.foreground)() } else { (h.background)() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_leaves_cores_to_other_apps() {
        assert_eq!((interactive_threads(10), interactive_threads(8)), (8, 6), "10-core M1 Max: 8");
        assert_eq!((interactive_threads(4), interactive_threads(2), interactive_threads(1), interactive_threads(0)), (3, 2, 1, 1));
        assert_eq!(interactive_threads(64), 62);
    }

    #[test]
    fn the_env_value_caps_the_default() {
        assert_eq!(cap(16, None), 16);
        assert_eq!(cap(16, Some("8")), 8);
        assert_eq!(cap(16, Some(" 4 ")), 4);
        assert_eq!(cap(4, Some("12")), 4, "a cap never raises the default");
        assert_eq!(cap(16, Some("0")), 16, "0 is no cap");
        assert_eq!(cap(16, Some("lots")), 16);
        assert_eq!(cap(16, Some("-3")), 16);
    }

    #[test]
    fn the_cli_cap_applies_to_the_per_core_default() {
        set_frame_threads(3);
        assert_eq!(frame_threads(64), 3);
        assert_eq!(frame_threads(1), 2, "under the cap: the default");
        set_frame_threads(0);
        if std::env::var_os(FRAME_THREADS_ENV).is_none() {
            assert_eq!(frame_threads(64), 16);
            assert_eq!(frame_threads(1), 2);
        }
    }
}
