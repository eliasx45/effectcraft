//! The mouse position in screen pixels, for when the windowing layer has none.
//!
//! While files are dragged in from the Finder, winit 0.30 on macOS delivers no cursor events (its
//! `NSDraggingDestination` only queues hovered, dropped and cancelled files), so the UI cannot learn
//! where a drop landed: egui's pointer is gone or stale (usually where it left the window).
//!
//! macOS needs no `unsafe` here: the `objc2-core-graphics` `CGEvent` calls are safe wrappers, read
//! only, and need no permission. Every other platform returns `None` and the UI falls back to the
//! pointer egui saw. Drop this once winit reports the pointer position during an OS drag-and-drop.
//! (Same technique as FilmCraft's `filmcraft_platform::cursor`.)
//!
//! Units: the result is in the physical pixels winit gives the window's inner position in, so the
//! UI divides it by its pixels per point and subtracts the window's origin in points. Core Graphics
//! reports global display points (origin at the top left of the main display, y down), the space
//! winit places the window in before scaling it by the window's backing scale factor. Scaling the
//! cursor by that same factor keeps the two in one space, so the point is exact on Retina and on
//! mixed-scale monitor setups alike, wherever the window and the cursor are.

/// The cursor's position on the virtual screen in physical pixels, or `None` where the OS cannot
/// say or this platform is not handled.
///
/// `window_scale` is the window's native pixels per point (winit's scale factor, egui's
/// `native_pixels_per_point`, not its zoomed `pixels_per_point`): macOS reports points and they
/// are scaled by it.
#[cfg(target_os = "macos")]
pub fn cursor_screen_position(window_scale: f32) -> Option<(i32, i32)> {
    use objc2_core_graphics::CGEvent;
    // An event made without a source carries the current cursor location, also while another
    // app (the Finder) runs a drag session.
    let event = CGEvent::new(None)?;
    let p = CGEvent::location(Some(&event));
    points_to_physical_px((p.x, p.y), f64::from(window_scale))
}

/// The cursor's position on the virtual screen in physical pixels: not known on this platform.
#[cfg(not(target_os = "macos"))]
pub fn cursor_screen_position(_window_scale: f32) -> Option<(i32, i32)> {
    None
}

/// A point in global display points as physical pixels at `scale` pixels per point, rounded to the
/// nearest pixel. `None` for a scale that is not a finite positive number, or a point that is not
/// finite or does not fit in `i32` pixels.
pub fn points_to_physical_px(points: (f64, f64), scale: f64) -> Option<(i32, i32)> {
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let px = |v: f64| {
        let p = (v * scale).round();
        (p.is_finite() && p >= f64::from(i32::MIN) && p <= f64::from(i32::MAX)).then_some(p as i32)
    };
    Some((px(points.0)?, px(points.1)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retina_points_become_twice_as_many_pixels() {
        assert_eq!(points_to_physical_px((640.0, 400.0), 2.0), Some((1280, 800)));
        // Core Graphics reports fractional points on Retina; they round to the nearest pixel.
        assert_eq!(points_to_physical_px((640.5, 400.25), 2.0), Some((1281, 801)));
        assert_eq!(points_to_physical_px((640.0, 400.0), 1.0), Some((640, 400)));
        assert_eq!(points_to_physical_px((100.0, 10.0), 1.5), Some((150, 15)));
    }

    #[test]
    fn displays_left_of_or_above_the_main_display_give_negative_pixels() {
        assert_eq!(points_to_physical_px((-1440.0, -200.5), 2.0), Some((-2880, -401)));
    }

    #[test]
    fn hostile_scales_and_points_give_none_instead_of_garbage() {
        for scale in [0.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(points_to_physical_px((10.0, 20.0), scale), None, "scale {scale}");
        }
        for p in [(f64::NAN, 0.0), (0.0, f64::INFINITY), (f64::MAX, 0.0), (0.0, -3.0e9)] {
            assert_eq!(points_to_physical_px(p, 2.0), None, "point {p:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reads_the_cursor_without_panicking_and_rejects_a_hostile_scale() {
        // Whether a position comes back depends on a window server session (none on some CI
        // runners), so only the hostile scale has a fixed answer.
        let _ = cursor_screen_position(2.0);
        assert_eq!(cursor_screen_position(f32::NAN), None);
        assert_eq!(cursor_screen_position(0.0), None);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_leave_it_to_the_windowing_layer() {
        assert_eq!(cursor_screen_position(1.0), None);
    }
}
