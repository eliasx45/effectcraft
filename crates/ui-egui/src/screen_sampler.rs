//! Sampling a colour anywhere on screen for the eyedroppers (the colour picker's and Effect
//! Controls'), as the system colour panel's loupe and Photoshop's eyedropper do.
//!
//! macOS: the system colour sampler (`NSColorSampler`, the loupe of the system colour panel) picks a
//! pixel anywhere: in any panel, another app or the desktop, without Screen Recording permission.
//! Its Objective-C call is `unsafe` in `objc2` (a block taking a raw `NSColor` pointer), which the
//! workspace forbids, so a helper process runs it: `osascript` with a JavaScript for Automation
//! script, which prints the colour in sRGB (or `cancel` when Escape ends the loupe).
//!
//! Elsewhere there is no system sampler: [`start`] returns false, and the colour picker samples
//! its own window instead (a press anywhere in it, see [`request_window_sample`]).

/// What a sampling session gave its owner: the colour (sRGB components 0–1), or nothing when it
/// was cancelled or failed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sampled(Option<[f32; 3]>);

fn running_key() -> egui::Id {
    egui::Id::new("screen-sampler-running")
}

fn result_key(owner: egui::Id) -> egui::Id {
    owner.with("screen-sample")
}

/// The system loupe is up (one at a time).
pub fn running(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(running_key())).unwrap_or(false)
}

/// Starts the system loupe for `owner` (any id; the same one takes the result with [`take`]).
/// Returns false where there is no system sampler (the caller falls back to its own sampling).
pub fn start(ctx: &egui::Context, owner: egui::Id) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    // A result left from an earlier session isn't this one's.
    ctx.data_mut(|d| d.remove::<Sampled>(result_key(owner)));
    if running(ctx) {
        return true;
    }
    ctx.data_mut(|d| d.insert_temp(running_key(), true));
    let ctx2 = ctx.clone();
    let spawned = std::thread::Builder::new().name("screen-sampler".into()).spawn(move || {
        let rgb = run_sampler();
        ctx2.data_mut(|d| {
            d.insert_temp(running_key(), false);
            d.insert_temp(result_key(owner), Sampled(rgb));
        });
        ctx2.request_repaint();
    });
    if spawned.is_err() {
        ctx.data_mut(|d| d.insert_temp(running_key(), false));
        return false;
    }
    true
}

/// The result of `owner`'s session, once it has ended: `Some(Some(rgb))` sampled, `Some(None)`
/// cancelled (or failed).
pub fn take(ctx: &egui::Context, owner: egui::Id) -> Option<Option<[f32; 3]>> {
    ctx.data_mut(|d| {
        let r = d.get_temp::<Sampled>(result_key(owner));
        d.remove::<Sampled>(result_key(owner));
        r.map(|s| s.0)
    })
}

/// The loupe: shown by a helper process, which runs its own event loop until a pixel is picked or
/// Escape cancels (and gives up after ten minutes).
#[cfg(target_os = "macos")]
const SCRIPT: &str = r#"
ObjC.import("AppKit");
var app = $.NSApplication.sharedApplication;
app.setActivationPolicy($.NSApplicationActivationPolicyAccessory);
app.finishLaunching;
var out = null;
$.NSColorSampler.new.showSamplerWithSelectionHandler(function (c) {
  try {
    var s = c.colorUsingColorSpace($.NSColorSpace.sRGBColorSpace);
    out = [s.redComponent, s.greenComponent, s.blueComponent].join(" ");
  } catch (e) { out = "cancel"; }
});
var until = $.NSDate.dateWithTimeIntervalSinceNow(600);
while (out === null && $.NSDate.date.compare(until) < 0) {
  var e = app.nextEventMatchingMaskUntilDateInModeDequeue($.NSEventMaskAny, $.NSDate.dateWithTimeIntervalSinceNow(0.05), $.NSDefaultRunLoopMode, true);
  if (e && !e.isNil()) app.sendEvent(e);
}
out === null ? "cancel" : out;
"#;

#[cfg(target_os = "macos")]
fn run_sampler() -> Option<[f32; 3]> {
    let out = std::process::Command::new("/usr/bin/osascript").args(["-l", "JavaScript", "-e", SCRIPT]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_rgb(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
fn run_sampler() -> Option<[f32; 3]> {
    None
}

/// "r g b" (0–1 each) as the helper prints it; anything else (`cancel`) is no colour.
fn parse_rgb(s: &str) -> Option<[f32; 3]> {
    let mut it = s.split_whitespace().map(|v| v.parse::<f32>().ok().filter(|f| f.is_finite()).map(|f| f.clamp(0.0, 1.0)));
    let rgb = [it.next()??, it.next()??, it.next()??];
    it.next().is_none().then_some(rgb)
}

/// The marker on a window screenshot taken to sample a pixel of it.
#[derive(Clone, Copy, Debug)]
struct WindowSample;

fn window_press_key() -> egui::Id {
    egui::Id::new("screen-sampler-window-press")
}

/// Fallback without a system sampler: asks for a screenshot of the window to sample the pixel at
/// `pos` (points) from; [`take_window_sample`] reads it when it arrives.
pub fn request_window_sample(ctx: &egui::Context, pos: egui::Pos2) {
    ctx.data_mut(|d| d.insert_temp(window_press_key(), pos));
    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(WindowSample)));
    ctx.request_repaint();
}

/// A window sample is on its way.
pub fn window_sample_pending(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<egui::Pos2>(window_press_key())).is_some()
}

/// The pixel a [`request_window_sample`] asked for, once its screenshot has arrived (sRGB 0–1).
pub fn take_window_sample(ctx: &egui::Context) -> Option<[f32; 3]> {
    let pos: egui::Pos2 = ctx.data(|d| d.get_temp(window_press_key()))?;
    let image = ctx.input(|i| {
        i.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { user_data, image, .. } if user_data.data.as_ref().is_some_and(|d| d.is::<WindowSample>()) => Some(image.clone()),
            _ => None,
        })
    })?;
    ctx.data_mut(|d| d.remove::<egui::Pos2>(window_press_key()));
    let ppp = ctx.pixels_per_point();
    let x = (pos.x * ppp).floor();
    let y = (pos.y * ppp).floor();
    if !(x.is_finite() && y.is_finite()) || x < 0.0 || y < 0.0 {
        return None;
    }
    let (x, y) = (x as usize, y as usize);
    let [w, h] = image.size;
    if x >= w || y >= h {
        return None;
    }
    let c = image.pixels.get(y.checked_mul(w)?.checked_add(x)?)?;
    Some([c.r(), c.g(), c.b()].map(|v| v as f32 / 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_helpers_colour_and_rejects_the_rest() {
        assert_eq!(parse_rgb("1 0.5 0.25\n"), Some([1.0, 0.5, 0.25]));
        assert_eq!(parse_rgb("1.2 -0.1 0.5"), Some([1.0, 0.0, 0.5]));
        assert_eq!(parse_rgb("cancel\n"), None);
        assert_eq!(parse_rgb("0.1 0.2"), None);
        assert_eq!(parse_rgb("0.1 0.2 0.3 0.4"), None);
        assert_eq!(parse_rgb("NaN 0 0"), None);
    }
}
