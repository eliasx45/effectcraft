//! RT10e: viewer frames with no copy. The GPU compositor runs on egui's own wgpu device, so a
//! frame it composites is drawn straight from its texture: no GPU→CPU readback and no CPU→GPU
//! upload (about 8 MB a frame at 1080p, 33 MB at 4K). See `frames::resident`.
//!
//! The host offers its device ([`crate::EffectcraftApp::share_display_device`]). A desktop window
//! comes with a [`TrafficLight`]: `Surface::configure` (a resize, a move to another display, a new
//! window) waits for the device to go idle and fails if anything is submitted meanwhile, so the
//! window's event path holds the light red around it and the compositor's submissions wait. A
//! host without a surface (the offscreen UI) offers its device without one.
//!
//! Off with Settings ▸ Previews ▸ Show GPU Frames Without Copying (live: frames are read back
//! again; the compositor moves to a device of its own at the next launch) or
//! `EFFECTCRAFT_ZERO_COPY=0` (the desktop app then starts exactly as before RT10e). The desktop
//! app also starts without it on an X server without DRI3 (Xvfb, a streamed cloud desktop),
//! where presenting the window holds up the shared queue (`EFFECTCRAFT_ZERO_COPY=1` overrides). When a
//! shared-device compositor retires (an uncaptured GPU error), the next one gets a device of its
//! own and frames are read back as before; a lost device closes the window after an auto-save
//! (`EffectcraftApp::close_after_device_loss`).

use std::sync::{Arc, OnceLock};

use effectcraft_engine::prefs::Prefs;
pub use effectcraft_gpu::TrafficLight;
pub use effectcraft_gpu::traffic_light::Red;

/// The kill switch: `0` (or `off`, `false`, `no`) reads every frame back as before; `1` (or
/// `on`, `true`, `yes`) also shares the offscreen UI's device.
pub const ENV: &str = "EFFECTCRAFT_ZERO_COPY";

/// [`ENV`]'s choice, read once.
pub fn env_choice() -> Option<bool> {
    static CHOICE: OnceLock<Option<bool>> = OnceLock::new();
    *CHOICE.get_or_init(|| parse_choice(std::env::var(ENV).ok().as_deref()))
}

fn parse_choice(v: Option<&str>) -> Option<bool> {
    match v?.trim().to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" => Some(false),
        "1" | "on" | "true" | "yes" => Some(true),
        _ => None,
    }
}

/// The host's offer and what became of it.
#[derive(Default)]
pub struct ZeroCopy {
    /// `Some(light)`: the host offers egui's device, with its window's traffic light (`None`
    /// for a host without a surface).
    offered: Option<Option<Arc<TrafficLight>>>,
    /// The compositor in use shares egui's device.
    pub(crate) active: bool,
    /// Its adapter has unified memory (GPU frames count against the RAM preview budget).
    pub(crate) unified: bool,
    /// A shared-device compositor retired: the next one gets a device of its own.
    pub(crate) failed: bool,
}

impl ZeroCopy {
    pub(crate) fn offer(&mut self, light: Option<Arc<TrafficLight>>) {
        self.offered = Some(light);
    }

    /// For the compositor about to be built: share egui's device (and submit through this
    /// light), or `None` for a device of its own.
    pub(crate) fn share(&self, prefs: &Prefs) -> Option<Option<Arc<TrafficLight>>> {
        if self.failed || env_choice() == Some(false) || !prefs.previews.zero_copy_display {
            return None;
        }
        self.offered.clone()
    }

    /// Whether viewer frames stay textures now (the setting applies live).
    pub(crate) fn resident(&self, prefs: &Prefs) -> bool {
        self.active && !self.failed && env_choice() != Some(false) && prefs.previews.zero_copy_display
    }

    /// The light, while the compositor submits through it.
    pub(crate) fn light(&self) -> Option<&Arc<TrafficLight>> {
        self.offered.as_ref().and_then(Option::as_ref).filter(|_| self.active)
    }

    /// Hold the light red while viewport `id` opens its window this frame (its surface is
    /// configured inside the frame, where the host's event path can't see it). `None` for a
    /// viewport that is already open, or without a light.
    pub(crate) fn hold_for_new_viewport(&self, ctx: &egui::Context, id: egui::ViewportId) -> Option<Red> {
        let light = self.light()?;
        let open = ctx.input(|i| i.raw.viewports.contains_key(&id));
        (!open).then(|| light.red())
    }
}

/// [`ZeroCopy::hold_for_new_viewport`] for the app's immediate viewports.
pub(crate) fn hold_for_new_viewport(app: &crate::EffectcraftApp, ctx: &egui::Context, id: egui::ViewportId) -> Option<Red> {
    app.zero_copy.hold_for_new_viewport(ctx, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_switch_values() {
        for off in ["0", "off", "FALSE", " no "] {
            assert_eq!(parse_choice(Some(off)), Some(false), "{off}");
        }
        for on in ["1", "on", "True", "yes"] {
            assert_eq!(parse_choice(Some(on)), Some(true), "{on}");
        }
        assert_eq!(parse_choice(Some("")), None);
        assert_eq!(parse_choice(Some("auto")), None);
        assert_eq!(parse_choice(None), None);
    }

    #[test]
    fn sharing_follows_the_offer_the_setting_and_failures() {
        let mut prefs = Prefs::default();
        assert!(prefs.previews.zero_copy_display, "on by default");
        let mut z = ZeroCopy::default();
        assert!(z.share(&prefs).is_none(), "no offer: a device of its own");
        let light = TrafficLight::new();
        z.offer(Some(light.clone()));
        if env_choice() == Some(false) {
            return;
        }
        assert!(z.share(&prefs).is_some_and(|l| l.is_some_and(|l| Arc::ptr_eq(&l, &light))));
        assert!(!z.resident(&prefs), "not until a compositor shares the device");
        z.active = true;
        assert!(z.resident(&prefs));
        assert!(z.light().is_some());
        prefs.previews.zero_copy_display = false;
        assert!(!z.resident(&prefs), "the setting applies live");
        assert!(z.share(&prefs).is_none());
        prefs.previews.zero_copy_display = true;
        z.failed = true;
        assert!(!z.resident(&prefs));
        assert!(z.share(&prefs).is_none(), "after a failure the compositor gets a device of its own");
    }

    /// A GPU error on the shared device retires the zero-copy compositor; the window still
    /// draws, so the next compositor gets a device of its own (frames read back as before). A
    /// lost device closes the window instead (no replacement).
    #[test]
    fn a_shared_device_failure_falls_back_to_the_readback_path() {
        use crate::{EffectcraftApp, gpu_failure::GpuFailureBridge};
        for lost in [false, true] {
            let mut app = EffectcraftApp::new(effectcraft_engine::Session::default());
            let ctx = egui::Context::default();
            let bridge = GpuFailureBridge::new(&ctx);
            app.set_gpu_failure_bridge(bridge.clone());
            app.share_display_device(Some(TrafficLight::new()));
            app.zero_copy.active = true;
            app.gpu_checked = true;
            bridge.report("uncaptured GPU error: synthetic", lost);
            app.apply_gpu_failure(&ctx);
            assert!(!app.zero_copy.active && app.zero_copy.failed);
            assert!(app.zero_copy.share(&app.session.prefs).is_none(), "the replacement gets a device of its own");
            #[cfg(not(target_arch = "wasm32"))]
            assert_eq!(app.zero_copy_frames(), 0);
            if lost {
                assert!(app.gpu_checked, "a lost device is not set up again");
                assert!(app.ui.status.contains("presentation requires restarting"));
            } else {
                assert!(!app.gpu_checked, "a compositor on a device of its own is set up next frame");
                assert!(app.ui.status.contains("copied for display again"), "{}", app.ui.status);
            }
        }
    }
}
