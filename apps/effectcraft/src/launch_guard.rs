//! Settings ▸ Startup & Repair ▸ Window Graphics, and the launch guard behind it.
//!
//! Some graphics drivers crash EffectCraft as its window's device is created (an access
//! violation in an older Intel UHD driver with DirectX 12 / Vulkan, #243), or a moment after
//! its first frame, as the window is shown and its surface re-created (AMD's Vulkan driver for
//! a Radeon RX 6800M, Adrenalin 26.9.2: an access violation in amdvlk64.dll on every launch).
//! A crash in a driver can't be caught, so each launch leaves a marker in the settings folder
//! until its window has kept drawing for [`SETTLE`] (or the app quits normally before then). A
//! marker left behind is a launch that stopped early, which can't tell a crashing driver from a
//! process killed or ended from outside (Task Manager, an agent restarting the app, a shutdown):
//! one such launch on its own switched a working Vulkan setup to OpenGL for good, and with it the
//! GPU compositor off. So the marker counts the launches in a row that stopped early, and only
//! [`STRIKES`] of them switch Window Graphics to OpenGL, which those drivers run, saying so once
//! the window is up (a driver that crashes does so on every launch). `WGPU_BACKEND` (wgpu's own override) still
//! wins over both. The marker is locked while its launch runs, so a second copy opened meanwhile
//! isn't taken for a crash; and a window OpenGL can't open either puts Window Graphics back to
//! Automatic ([`opengl_failed`]) rather than failing every launch.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use effectcraft_engine::prefs::{PREFS_FILE, Prefs};
use eframe::wgpu::Backends;

/// Left in the settings folder from a launch until its window has settled.
const MARKER: &str = "launch-pending";

/// How long the window keeps drawing after its first frame before the launch counts as made.
/// A driver that fails as the window is shown and its surface re-created does so within the
/// first second; a user quitting sooner than this is told the next launch uses OpenGL and can
/// switch back in Settings.
pub const SETTLE: Duration = Duration::from_secs(3);

/// Launches in a row that stop before settling before Window Graphics switches to OpenGL: one
/// alone is as likely a kill from outside as a crashing driver.
pub const STRIKES: u32 = 2;

/// This launch's window graphics.
#[derive(Debug, Default)]
pub struct Launch {
    /// The graphics APIs the window may use (`None`: eframe's default).
    pub backends: Option<Backends>,
    /// Switched to OpenGL because the previous launch stopped as its window opened: the message
    /// to show.
    pub notice: Option<String>,
    marker: Option<PathBuf>,
    /// Holds the marker's lock until the launch settles (or the process ends).
    lock: Option<File>,
    /// When the window drew its first frame, until the launch settles.
    first_frame: Option<Instant>,
}

impl Launch {
    /// Read Window Graphics in the settings folder `dir`, switch it to OpenGL when the previous
    /// [`STRIKES`] launches stopped as their window opened (where OpenGL exists: not macOS), and
    /// leave this launch's marker. `env_override`: `WGPU_BACKEND` is set.
    pub fn begin(dir: Option<&Path>, env_override: bool) -> Launch {
        let Some(dir) = dir.filter(|_| !env_override) else { return Launch::default() };
        let prefs_path = dir.join(PREFS_FILE);
        let mut prefs = std::fs::read_to_string(&prefs_path).map(|t| Prefs::from_json(&t)).unwrap_or_default();
        let marker = dir.join(MARKER);
        // Locked by another launch that is still opening its window: not a crash.
        let held = File::open(&marker).is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        // Launches in a row before this one that stopped before settling (a marker from an
        // older version holds no count: one).
        let stopped =
            if marker.exists() && !held { std::fs::read_to_string(&marker).ok().and_then(|t| t.trim().parse::<u32>().ok()).unwrap_or(1).max(1) } else { 0 };
        let mut notice = None;
        // This launch's count, kept in its marker until it settles.
        let mut strikes = stopped.saturating_add(1);
        let guarded = prefs.startup.window_graphics != "gl" && cfg!(not(target_os = "macos"));
        if stopped >= STRIKES && guarded {
            prefs.startup.window_graphics = "gl".into();
            strikes = 1;
            match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&prefs_path, prefs.to_json())) {
                Ok(()) => log::warn!("the last {stopped} launches stopped as their window opened: switching Window Graphics to OpenGL"),
                Err(e) => log::warn!("the last {stopped} launches stopped as their window opened; saving Window Graphics failed: {e}"),
            }
            notice = Some(
                "EffectCraft's last launches stopped as their window opened, so it now draws with OpenGL, \
                 which renders previews on the CPU (Settings ▸ Startup & Repair ▸ Window Graphics). Updating the \
                 graphics driver may let it use Automatic again."
                    .to_string(),
            );
        } else if stopped > 0 && guarded {
            log::warn!(
                "the last launch stopped within {} s of opening its window (a crash, or the process was ended): \
                 Window Graphics switches to OpenGL if {} launches in a row do",
                SETTLE.as_secs(),
                STRIKES
            );
        }
        let backends = backends_for(&prefs.startup.window_graphics);
        if held {
            // The first copy's marker stays its own.
            return Launch { backends, notice, ..Launch::default() };
        }
        let (marker, lock) = match std::fs::create_dir_all(dir).and_then(|()| File::create(&marker)) {
            // Where locks aren't available the guard works as before.
            Ok(mut f) => {
                use std::io::Write;
                if let Err(e) = write!(f, "{strikes}").and_then(|()| f.flush()) {
                    log::warn!("launch marker: {e}");
                }
                (Some(marker), f.try_lock().is_ok().then_some(f))
            }
            Err(e) => {
                log::warn!("launch marker: {e}");
                (None, None)
            }
        };
        Launch { backends, notice, marker, lock, first_frame: None }
    }

    /// The window drew a frame. Until the launch has settled, returns how long that still takes
    /// (ask for a repaint then, so that an idle window settles too).
    pub fn frame_drawn(&mut self) -> Option<Duration> {
        self.frame_drawn_at(Instant::now())
    }

    fn frame_drawn_at(&mut self, now: Instant) -> Option<Duration> {
        self.marker.as_ref()?;
        let first = *self.first_frame.get_or_insert(now);
        let left = SETTLE.saturating_sub(now.saturating_duration_since(first));
        if left.is_zero() {
            self.settled();
            return None;
        }
        Some(left)
    }

    /// The app quits normally: however soon after opening, that's no crash.
    pub fn exited(&mut self) {
        self.settled();
    }

    /// The launch made it: release and remove its marker.
    fn settled(&mut self) {
        self.lock = None;
        if let Some(m) = self.marker.take()
            && let Err(e) = std::fs::remove_file(&m)
        {
            log::warn!("launch marker: {e}");
        }
    }
}

/// The graphics APIs Window Graphics `choice` lets the window use (`None`: eframe's default).
pub fn backends_for(choice: &str) -> Option<Backends> {
    match choice {
        "gl" => Some(Backends::GL),
        // Not on macOS, where wgpu has no Vulkan: Automatic (Metal) there.
        "vulkan" if cfg!(not(target_os = "macos")) => Some(Backends::VULKAN),
        _ => None,
    }
}

/// The window couldn't open (an error, not a kill from outside): the next launch switches to
/// OpenGL at once, without waiting for [`STRIKES`] launches in a row.
pub fn window_failed(dir: Option<&Path>) {
    let Some(marker) = dir.map(|d| d.join(MARKER)).filter(|m| m.exists()) else { return };
    if let Err(e) = std::fs::write(&marker, STRIKES.to_string()) {
        log::warn!("launch marker: {e}");
    }
}

/// The window couldn't open with OpenGL (an error, not a crash): back to Automatic, and no marker.
pub fn opengl_failed(dir: Option<&Path>) {
    let Some(dir) = dir else { return };
    let prefs_path = dir.join(PREFS_FILE);
    let mut prefs = std::fs::read_to_string(&prefs_path).map(|t| Prefs::from_json(&t)).unwrap_or_default();
    prefs.startup.window_graphics = "auto".into();
    if let Err(e) = std::fs::write(&prefs_path, prefs.to_json()) {
        log::warn!("saving Window Graphics failed: {e}");
    }
    let _ = std::fs::remove_file(dir.join(MARKER));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_graphics(dir: &Path) -> String {
        Prefs::from_json(&std::fs::read_to_string(dir.join(PREFS_FILE)).unwrap_or_default()).startup.window_graphics
    }

    /// A launch that stopped before settling (a driver crash, or the process ended from outside).
    fn stops_early(dir: &Path) -> Launch {
        let l = Launch::begin(Some(dir), false);
        assert!(dir.join(MARKER).exists());
        l
    }

    /// Launches that never drew switch the next one to OpenGL, which then stays (#243), once
    /// [`STRIKES`] of them stopped in a row.
    #[test]
    fn launches_that_never_drew_switch_the_next_to_opengl() {
        let dir = std::env::temp_dir().join(format!("ec-launch-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // A first launch that draws: nothing changes. Its first frames don't settle it (a driver
        // may still crash as the window is shown); frames for `SETTLE` do.
        let mut l = Launch::begin(Some(&dir), false);
        assert_eq!((l.backends, l.notice.is_some()), (None, false));
        assert!(dir.join(MARKER).exists());
        let t0 = Instant::now();
        assert_eq!(l.frame_drawn_at(t0), Some(SETTLE));
        assert!(l.frame_drawn_at(t0 + SETTLE / 2).is_some_and(|left| left <= SETTLE / 2));
        assert!(dir.join(MARKER).exists());
        assert_eq!(l.frame_drawn_at(t0 + SETTLE), None);
        assert!(!dir.join(MARKER).exists());
        assert_eq!(l.frame_drawn_at(t0 + SETTLE), None, "settled stays settled");
        // Launches that crash before drawing (they never call `drawn`)…
        drop(stops_early(&dir));
        drop(stops_early(&dir));
        // …make the next one use OpenGL and say so, once.
        let mut l = Launch::begin(Some(&dir), false);
        if cfg!(target_os = "macos") {
            assert_eq!((l.backends, l.notice.is_some()), (None, false), "no OpenGL on macOS");
        } else {
            assert_eq!(l.backends, Some(Backends::GL));
            assert!(l.notice.is_some());
            assert_eq!(window_graphics(&dir), "gl", "saved in Settings");
            l.exited();
            let l = Launch::begin(Some(&dir), false);
            assert_eq!((l.backends, l.notice.is_some()), (Some(Backends::GL), false), "it stays, without the notice");
        }
        // `WGPU_BACKEND` wins, and leaves no marker.
        let _ = std::fs::remove_file(dir.join(MARKER));
        assert_eq!(Launch::begin(Some(&dir), true).backends, None);
        assert!(!dir.join(MARKER).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Launches that stop moments after their first frame (AMD's Vulkan driver crashing as the
    /// window is shown and its surface re-created) switch the next one to OpenGL too; a normal
    /// quit in that time doesn't.
    #[test]
    fn launches_that_stop_after_their_first_frame_switch_the_next_to_opengl() {
        let dir = std::env::temp_dir().join(format!("ec-launch-guard-3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for _ in 0..STRIKES {
            let mut l = Launch::begin(Some(&dir), false);
            assert_eq!(l.backends, None);
            assert!(l.frame_drawn().is_some());
        }
        let l = Launch::begin(Some(&dir), false);
        if cfg!(target_os = "macos") {
            assert_eq!(l.backends, None, "no OpenGL on macOS");
        } else {
            assert_eq!(l.backends, Some(Backends::GL));
            assert!(l.notice.is_some());
        }
        drop(l);
        let _ = std::fs::remove_dir_all(&dir);
        // Quitting right after the window opened leaves no marker.
        let mut l = Launch::begin(Some(&dir), false);
        assert!(l.frame_drawn().is_some());
        l.exited();
        assert!(!dir.join(MARKER).exists());
        assert_eq!(Launch::begin(Some(&dir), false).backends, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One launch ended from outside right after its window opened (killed by an agent
    /// restarting it, Task Manager, a shutdown) looks like a crash, but alone it doesn't switch
    /// Window Graphics: on 16 October 2026 that turned a working Vulkan PC's preview into CPU
    /// rendering until Settings were reset. A launch that settles in between starts the count
    /// again; a marker from an older version (empty) counts as one.
    #[test]
    fn one_launch_killed_early_doesnt_switch_to_opengl() {
        let dir = std::env::temp_dir().join(format!("ec-launch-guard-4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        drop(stops_early(&dir));
        let mut l = Launch::begin(Some(&dir), false);
        assert_eq!((l.backends, l.notice.is_some()), (None, false), "one early stop: still Automatic");
        assert_eq!(window_graphics(&dir), "auto");
        // It settles: the count starts again.
        let t0 = Instant::now();
        l.frame_drawn_at(t0);
        assert_eq!(l.frame_drawn_at(t0 + SETTLE), None);
        drop(stops_early(&dir));
        assert_eq!(Launch::begin(Some(&dir), false).backends, None, "not two in a row");
        // An older version's marker holds no count: it is one launch.
        let _ = std::fs::remove_file(dir.join(MARKER));
        std::fs::write(dir.join(MARKER), "").unwrap();
        drop(Launch::begin(Some(&dir), false));
        let l = Launch::begin(Some(&dir), false);
        if cfg!(not(target_os = "macos")) {
            assert_eq!(l.backends, Some(Backends::GL), "the old marker and this stop make two");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A second copy opened while the first is still opening isn't a crash; OpenGL that can't
    /// open a window either goes back to Automatic instead of failing every launch.
    #[test]
    fn a_second_copy_or_failed_opengl_doesnt_strand_window_graphics() {
        let dir = std::env::temp_dir().join(format!("ec-launch-guard-2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        drop(stops_early(&dir));
        let first = Launch::begin(Some(&dir), false);
        for _ in 0..STRIKES {
            assert_eq!(Launch::begin(Some(&dir), false).backends, None);
        }
        assert_eq!(window_graphics(&dir), "auto");
        drop(first);
        if cfg!(not(target_os = "macos")) {
            assert_eq!(Launch::begin(Some(&dir), false).backends, Some(Backends::GL), "real crashes still switch");
            opengl_failed(Some(&dir));
            assert_eq!(window_graphics(&dir), "auto");
            assert_eq!(Launch::begin(Some(&dir), false).backends, None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A window that couldn't open (an error) switches the next launch at once.
    #[test]
    fn a_window_that_failed_to_open_switches_the_next_launch() {
        let dir = std::env::temp_dir().join(format!("ec-launch-guard-5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        drop(stops_early(&dir));
        window_failed(Some(&dir));
        let l = Launch::begin(Some(&dir), false);
        assert_eq!(l.backends, if cfg!(target_os = "macos") { None } else { Some(Backends::GL) });
        // Without a marker (`WGPU_BACKEND`) it leaves none.
        let _ = std::fs::remove_dir_all(&dir);
        window_failed(Some(&dir));
        assert!(!dir.join(MARKER).exists());
    }

    /// Window Graphics' Vulkan choice asks for Vulkan alone (not on macOS, which has none).
    #[test]
    fn window_graphics_choices_pick_backends() {
        assert_eq!(backends_for("auto"), None);
        assert_eq!(backends_for("gl"), Some(Backends::GL));
        assert_eq!(backends_for("vulkan"), if cfg!(target_os = "macos") { None } else { Some(Backends::VULKAN) });
    }
}
