//! Automatic proxy suggestions (PROXY-AUTO1): the trigger rules, a "No" saved in the project,
//! "Yes" making and setting a preview-only proxy, and final renders keeping the original.

use std::sync::Arc;

use effectcraft_project::render_queue::ProxyUse;
use effectcraft_project::{Footage, FootageKind, ItemId, Project};
use effectcraft_raster::Image;
use effectcraft_time::{FrameRate, Tick};
use serde_json::json;

use crate::Session;
use crate::auto_proxy::{ProxySuggestion, Reason};
use crate::render::{FootageSource, RenderOpts, Renderer};

fn name_of(path: &str) -> &str {
    std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// `big.mov` 96×54 (3× its comp), `fit.mov` / `smoke.mov` 40×24, `still.png` 96×54; proxies
/// (`*_proxy.mp4`) 32×18. Originals are red, proxies blue.
fn size_of(path: &str) -> (u32, u32) {
    match name_of(path) {
        n if n.ends_with("_proxy.mp4") => (32, 18),
        "big.mov" | "still.png" => (96, 54),
        _ => (40, 24),
    }
}

/// The exact view: every file decoded as it is.
struct Files;

impl FootageSource for Files {
    fn frame(&self, _: ItemId, f: &Footage, _t: Tick) -> Option<Arc<Image>> {
        let (w, h) = size_of(&f.path);
        let mut img = Image::new(w, h);
        let px = if name_of(&f.path).ends_with("_proxy.mp4") { [0.0, 0.0, 1.0, 1.0] } else { [1.0, 0.0, 0.0, 1.0] };
        for y in 0..h {
            for x in 0..w {
                img.set(x, y, px);
            }
        }
        Some(Arc::new(img))
    }
}

/// A preview view (the media pool's): the same files, with [`Files`] as its exact twin.
struct Preview(Files);

impl FootageSource for Preview {
    fn frame(&self, i: ItemId, f: &Footage, t: Tick) -> Option<Arc<Image>> {
        self.0.frame(i, f, t)
    }
    fn exact(&self) -> Option<&dyn FootageSource> {
        Some(&self.0)
    }
}

struct Probe;

impl crate::Importer for Probe {
    fn probe(&self, path: &str) -> Result<Footage, String> {
        let (w, h) = size_of(path);
        let video = path.ends_with(".mov") || path.ends_with(".mp4");
        Ok(Footage {
            path: path.into(),
            kind: if video { FootageKind::Video } else { FootageKind::Still },
            width: w,
            height: h,
            has_video: true,
            frame_rate: FrameRate::from_f64(10.0),
            duration: Tick::from_seconds_f64(if video { 2.0 } else { 0.0 }),
            ..Default::default()
        })
    }
    fn hardware_refuses(&self, f: &Footage) -> bool {
        name_of(&f.path) == "smoke.mov"
    }
    fn build_proxy(&self, src: &str, out: &str, w: u32, h: u32, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<(), String> {
        assert_ne!(src, out, "the original is never written");
        assert_eq!((w, h), (32, 18), "the largest using comp's size");
        let out = std::path::Path::new(out);
        std::fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(out, b"proxy").map_err(|e| e.to_string())?;
        progress(1, 1);
        Ok(())
    }
}

fn session() -> Session {
    Session { footage: Arc::new(Preview(Files)), importer: Some(Arc::new(Probe)), ..Default::default() }
}

fn import(s: &mut Session, path: &str) -> ItemId {
    ItemId(s.execute("file.import", json!({"paths": [path]})).unwrap()["items"][0].as_u64().unwrap())
}

/// A 32×18, 10 fps comp (a 100 ms frame budget) holding `items`; its id and layer ids.
fn comp_with(s: &mut Session, items: &[ItemId]) -> (ItemId, Vec<effectcraft_project::LayerId>) {
    s.execute("comp.new", json!({"name": "C", "width": 32, "height": 18, "frameRate": 10, "duration": 1})).unwrap();
    let c = s.active_comp_id().unwrap();
    let mut layers = vec![];
    for i in items {
        s.execute("layer.addItem", json!({"item": i.0})).unwrap();
        layers.push(s.active_comp().unwrap().layers[0].id);
    }
    (c, layers)
}

fn suggested(s: &Session, item: ItemId) -> Option<ProxySuggestion> {
    s.proxy_suggestions().into_iter().find(|p| p.item == item)
}

#[test]
fn the_question_reads_like_the_brief() {
    let p = ProxySuggestion {
        item: ItemId(1),
        name: "Smoke".into(),
        width: 6144,
        height: 3456,
        proxy_width: 1920,
        proxy_height: 1080,
        comp: ItemId(2),
        reasons: vec![Reason::Size, Reason::Hardware, Reason::Slow],
        ms_per_frame: Some(46.2),
        budget_ms: Some(41.7),
    };
    assert_eq!(p.question(), "Smoke is 6K and costs ~46 ms/frame here. Make a 1080p proxy?");
    let unmeasured = ProxySuggestion { ms_per_frame: None, budget_ms: None, reasons: vec![Reason::Hardware], ..p.clone() };
    assert_eq!(unmeasured.question(), "Smoke is 6K and the hardware decoder can't take it. Make a 1080p proxy?");
    assert_eq!(crate::auto_proxy::fit_size(6144, 3456, 1920, 1080), (1920, 1080));
    assert_eq!(crate::auto_proxy::fit_size(6144, 3456, 1080, 1920), (1080, 608));
}

#[test]
fn size_hardware_and_measured_time_trigger_a_suggestion() {
    let mut s = session();
    let big = import(&mut s, "/m/big.mov");
    let fit = import(&mut s, "/m/fit.mov");
    let smoke = import(&mut s, "/m/smoke.mov");
    let still = import(&mut s, "/m/still.png");
    let unused = import(&mut s, "/m/other/big.mov");
    let (c, layers) = comp_with(&mut s, &[big, fit, smoke, still]);
    let _ = c;

    // 3× the comp: suggested at the comp's size.
    let b = suggested(&s, big).expect("big footage is suggested");
    assert_eq!(b.reasons, vec![Reason::Size]);
    assert_eq!((b.proxy_width, b.proxy_height), (32, 18));
    assert_eq!(b.comp, c);
    // 1.25× and decoded in hardware, not measured: nothing to ask.
    assert!(suggested(&s, fit).is_none());
    // The hardware decoder refuses it.
    assert_eq!(suggested(&s, smoke).unwrap().reasons, vec![Reason::Hardware]);
    // Stills and footage no comp uses are not offered proxies.
    assert!(suggested(&s, still).is_none());
    assert!(suggested(&s, unused).is_none());

    // Measured: a few slow frames don't count, a section of them does; fast frames never.
    let fit_layer = layers[1];
    for _ in 0..3 {
        s.note_layer_costs(c, [(c, fit_layer, 160.0)]);
    }
    assert!(suggested(&s, fit).is_none(), "not enough frames measured");
    for _ in 0..10 {
        s.note_layer_costs(c, [(c, fit_layer, 160.0)]);
    }
    let f = suggested(&s, fit).expect("slower than the 100 ms budget");
    assert_eq!(f.reasons, vec![Reason::Slow]);
    assert!((f.ms_per_frame.unwrap() - 160.0).abs() < 1e-6);
    assert!(f.question().contains("costs ~160 ms/frame here"), "{}", f.question());

    // An item with a proxy is never suggested.
    s.execute("file.setProxy", json!({"item": big.0, "path": "/m/big_32x18_proxy.mp4"})).unwrap();
    assert!(suggested(&s, big).is_none());

    let listed = s.execute("proxy.suggest", json!({})).unwrap();
    let ids: Vec<u64> = listed["candidates"].as_array().unwrap().iter().map(|c| c["item"].as_u64().unwrap()).collect();
    assert!(ids.contains(&smoke.0) && ids.contains(&fit.0) && !ids.contains(&big.0), "{listed}");
}

#[test]
fn no_is_saved_in_the_project() {
    let mut s = session();
    let big = import(&mut s, "/m/big.mov");
    comp_with(&mut s, &[big]);
    assert!(suggested(&s, big).is_some());
    s.execute("proxy.dismissSuggestion", json!({"item": big.0})).unwrap();
    assert!(suggested(&s, big).is_none());
    // Saved with the project: a reopened project doesn't ask again.
    let saved = serde_json::to_string(&*s.project).unwrap();
    let reopened: Project = serde_json::from_str(&saved).unwrap();
    assert!(reopened.settings.proxy_hint_dismissed.contains(&big));
    let mut t = session();
    t.project = Arc::new(reopened);
    assert!(suggested(&t, big).is_none());
    // Undoable, and junk is refused.
    s.execute("edit.undo", json!({})).unwrap();
    assert!(suggested(&s, big).is_some());
    assert!(s.execute("proxy.dismissSuggestion", json!({"item": 999_999})).is_err());
    assert!(s.execute("proxy.dismissSuggestion", json!({"item": "x"})).is_err());
    // Projects saved before carry no field (and none is written while nothing is dismissed).
    assert!(!serde_json::to_string(&*s.project).unwrap().contains("proxy_hint_dismissed"));
}

fn blue(p: [f32; 4]) -> bool {
    p[2] > 0.99 && p[0] < 0.01
}
fn red(p: [f32; 4]) -> bool {
    p[0] > 0.99 && p[2] < 0.01
}

/// The comp's centre as a preview renders it (the session's preview view of the footage).
fn preview(s: &Session, c: ItemId, o: RenderOpts) -> [f32; 4] {
    Renderer::new(&s.project, s.footage.as_ref(), o).comp_frame(c, Tick::ZERO).get(16, 9)
}

/// The comp's centre as a final render (Render Queue, export, Save Frame As) renders it: through
/// the exact view, as `effectcraft_export::Job::source` does.
fn fin(s: &Session, c: ItemId, o: RenderOpts) -> [f32; 4] {
    Renderer::new(&s.project, crate::render::exact_footage(s.footage.as_ref()), o).comp_frame(c, Tick::ZERO).get(16, 9)
}

#[test]
fn yes_makes_a_preview_only_proxy_and_final_renders_keep_the_original() {
    let dir = std::env::temp_dir().join(format!("ec-auto-proxy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut s = session();
    let big = import(&mut s, "/m/big.mov");
    let (c, _) = comp_with(&mut s, &[big]);

    // An unsaved project has no folder for proxies.
    assert!(s.execute("proxy.autoCreate", json!({"item": big.0, "wait": true})).is_err());
    s.path = Some(dir.join("Edit.ecproj").to_string_lossy().into_owned());
    let r = s.execute("proxy.autoCreate", json!({"item": big.0, "wait": true})).unwrap();
    let px = s.project.item(big).unwrap().proxy.clone().expect("the proxy is set");
    assert!(px.auto && px.enabled, "{r}");
    assert_eq!(px.footage.path, dir.join("Proxies").join("big_32x18_proxy.mp4").to_string_lossy());
    assert!(std::path::Path::new(&px.footage.path).exists());
    assert!(suggested(&s, big).is_none(), "asked once");
    // The mark survives save / load.
    let saved = serde_json::to_string(&*s.project).unwrap();
    let reopened: Project = serde_json::from_str(&saved).unwrap();
    assert!(reopened.item(big).unwrap().proxy.as_ref().unwrap().auto);

    let opts = RenderOpts::default();
    assert_eq!(opts.proxy, ProxyUse::CurrentSettings);
    // Preview (the viewer reads through a preview view): the proxy.
    assert!(blue(preview(&s, c, opts)));
    assert!(blue(s.render(c, Tick::ZERO, opts).get(16, 9)));
    // Final renders read the exact view: the original under Current Settings and Use No /
    // Comp Proxies Only; only Use All Proxies takes the automatic proxy.
    assert!(red(fin(&s, c, opts)), "Render Queue / export with Current Settings use the original");
    assert!(red(fin(&s, c, RenderOpts { proxy: ProxyUse::UseNone, ..opts })));
    assert!(red(fin(&s, c, RenderOpts { proxy: ProxyUse::UseCompOnly, ..opts })));
    assert!(blue(fin(&s, c, RenderOpts { proxy: ProxyUse::UseAll, ..opts })));
    // A plain source (no preview view, e.g. a headless render) is a final render too.
    assert!(red(Renderer::new(&s.project, &Files, opts).comp_frame(c, Tick::ZERO).get(16, 9)));
    // Use Proxy off: the preview shows the original as well.
    s.execute("file.useProxy", json!({"item": big.0})).unwrap();
    assert!(red(preview(&s, c, opts)));

    // A hand-set proxy is unchanged: Current Settings renders use it everywhere.
    s.execute("file.setProxy", json!({"item": big.0, "path": "/m/big_32x18_proxy.mp4"})).unwrap();
    assert!(!s.project.item(big).unwrap().proxy.as_ref().unwrap().auto);
    assert!(blue(fin(&s, c, opts)));
    let _ = std::fs::remove_dir_all(&dir);
}
