//! The UI command dispatcher (`menus/*.rs` area files, see [`super::dispatch`]) handles exactly the
//! ids the old central `match` in `menus.rs` handled, and no id is claimed by two area files.

use super::dispatch::{self, Claim, Stage};

/// The ids `menus::invoke` handled itself before the split (M1), in the old match order: `x.*` is
/// a prefix (`tool.*` is every `tool.<name>`). Captured from `menus.rs` before the refactor. Add
/// an id here when you add one to an area file.
const PINNED_INVOKE: &[&str] = &[
    "window.panel.*",
    "window.workspace.*",
    "tool.*",
    "viewer.nudge.*",
    "timeline.reveal.*",
    "timeline.keyAt.*",
    "timeline.revealAdd.*",
    "anim.reveal",
    "file.close",
    "window.maximizePanel",
    "window.dockAction",
    "window.dockPanel",
    "window.floatPanel",
    "window.closePanel",
    "window.closeOtherPanels",
    "window.closePanelGroup",
    "window.floatPanelGroup",
    "window.stackPanelGroup",
    "window.soloPanels",
    "window.smallTabs",
    "window.panelMenu",
    "markers.dialog",
    "view.theme.*",
    "playback.ramPreview",
    "playback.preview.shiftSpacebar",
    "playback.preview.shiftNumpad0",
    "playback.preview.altNumpad0",
    "playback.stop",
    "view.fit",
    "view.fitUpTo100",
    "view.nextItem",
    "view.previousItem",
    "view.actualSize",
    "view.safeMargins",
    "view.transparencyGrid",
    "view.fastPreviews",
    "timeline.zoomIn",
    "timeline.zoomOut",
    "timeline.zoomFit",
    "timeline.scrollToTime",
    "timeline.scrollToLayer",
    "timeline.parentColumn",
    "timeline.zoomFrameToggle",
    "timeline.column",
    "timeline.columnLayout",
    "project.column",
    "timeline.sourceName",
    "flowchart.options",
    "flowchart.graph",
    "timeline.search",
    "timeline.graphEditor",
    "timeline.switchesModes",
    "timeline.collapseAll",
    "timeline.twirlSelected",
    "timeline.workAreaBegin",
    "timeline.workAreaEnd",
    "timeline.moveInToTime",
    "timeline.moveOutToTime",
    "timeline.trimInToTime",
    "timeline.trimOutToTime",
    "app.home",
    "app.newComp",
    "comp.new",
    "app.compSettings",
    "comp.settings",
    "app.solidSettings",
    "layer.newSolid",
    "keys.velocity",
    "keys.interpolation",
    "layer.timeStretch",
    "window.maximizeApp",
    "window.toggleCompTimeline",
    "window.nextPanel",
    "window.previousPanel",
    "window.closeAll",
    "window.panelWindow",
    "view.theme.toggle",
    "project.deleteWithoutConfirmation",
    "motionLibrary.view",
    "motionLibrary.apply",
    "motionLibrary.copyName",
    "motionLibrary.save",
    "motionLibrary.folder",
    "motionLibrary.saveSelection",
    "motionLibrary.card",
    "motionLibrary.group",
    "motionLibrary.importFiles",
    "motionLibrary.refresh",
    "motionLibrary.curve",
    "motionLibrary.tool",
    "viewer.freeTransform",
];

/// The ids `menus::frontend` handled (frontend events of engine commands), in the old match order.
const PINNED_FRONTEND: &[&str] = &[
    "effects.copyPresetsFolderPath",
    "mediaBrowser.copySourcePath",
    "app.about",
    "prop.editGradient",
    "playback.scrubAudio",
    "view.newViewer",
    "window.scriptPanel",
    "layer.style.options",
    "app.settings",
    "app.gpuInfo",
    "app.hide",
    "app.hideOthers",
    "app.showAll",
    "app.quit",
    "app.commandPalette",
    "app.fxConsole",
    "app.keyboardShortcuts",
    "app.templates",
    "renderQueue.notify",
    "app.find",
    "playback.toggle",
    "playback.cacheWhenIdle",
    "playback.audio",
    "view.zoomIn",
    "view.zoomOut",
    "view.res.custom",
    "view.rulers",
    "view.guides",
    "view.snapToGuides",
    "view.lockGuides",
    "view.grid",
    "view.snapToGrid",
    "view.layerControls",
    "view.sharpZoom",
    "view.options",
    "view.panelBackground",
    "view.fullScreen",
    "window.panel",
    "window.workspace",
    "window.resetWorkspace",
    "window.saveWorkspace",
    "window.saveWorkspaceAs",
    "window.editWorkspaces",
    "view.lookAt",
    "comp.flowchart",
    "comp.miniFlowchart",
    "track.editTargetDialog",
    "track.optionsDialog",
    "layer.openLayer",
    "app.confirmExecute",
    "app.showReport",
    "window.assignWorkspaceShortcut",
    "file.interpretFootage",
    "help.inAppTutorials",
    "file.newFromTemplate",
    "effect.manage",
    "anim.browsePresets",
    "timeline.revealProps",
    "window.maximizeApp",
    "project.deleteWithoutConfirmation",
];

fn pinned(stage: Stage) -> &'static [&'static str] {
    match stage {
        Stage::Invoke => PINNED_INVOKE,
        Stage::Frontend => PINNED_FRONTEND,
    }
}

fn shown(c: Claim) -> String {
    match c {
        Claim::Id(id) => id.to_string(),
        Claim::Prefix(p) => format!("{p}*"),
    }
}

/// `(area file, claim)` of every handler of `stage`.
fn claims(stage: Stage) -> Vec<(&'static str, Claim)> {
    dispatch::all().filter(|(_, h)| h.stage == stage).map(|(area, h)| (area, h.claim)).collect()
}

#[test]
fn handles_exactly_the_pinned_ids() {
    for stage in [Stage::Invoke, Stage::Frontend] {
        let mut got: Vec<String> = claims(stage).into_iter().map(|(_, c)| shown(c)).collect();
        let mut want: Vec<String> = pinned(stage).iter().map(|s| s.to_string()).collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "{stage:?}: the handled ids changed; update PINNED_* only for a deliberate change");
    }
    assert_eq!(PINNED_INVOKE.len() + PINNED_FRONTEND.len(), 151);
}

#[test]
fn no_id_is_claimed_by_two_area_files() {
    for stage in [Stage::Invoke, Stage::Frontend] {
        let all = claims(stage);
        for (i, (area, c)) in all.iter().enumerate() {
            for (other, d) in all.iter().skip(i + 1) {
                assert_ne!(c, d, "{stage:?}: `{}` is claimed twice ({area}, {other})", shown(*c));
                // A prefix covering another area's id or prefix is a double claim too (an exact id
                // under a prefix of its own area file is a deliberate special case).
                let covers = |a: &Claim, b: &Claim| match (a, b) {
                    (Claim::Prefix(p), Claim::Id(x) | Claim::Prefix(x)) => x.starts_with(p),
                    _ => false,
                };
                if area != other {
                    assert!(!covers(c, d) && !covers(d, c), "{stage:?}: `{}` ({area}) and `{}` ({other}) overlap", shown(*c), shown(*d));
                }
            }
        }
    }
}

/// Exact ids first, then the longest matching prefix; everything else is not a UI command.
#[test]
fn lookup_precedence() {
    for stage in [Stage::Invoke, Stage::Frontend] {
        for s in pinned(stage) {
            let (id, want) = match s.strip_suffix('*') {
                Some(p) => (format!("{p}someName"), Claim::Prefix(p)),
                None => (s.to_string(), Claim::Id(s)),
            };
            assert_eq!(dispatch::find(stage, &id).map(|h| h.claim), Some(want), "{stage:?} {id}");
        }
    }
    assert!(dispatch::find(Stage::Invoke, "layer.newText").is_none());
    assert!(dispatch::find(Stage::Invoke, "window.panel").is_none());
    assert!(dispatch::find(Stage::Frontend, "window.panel.project").is_none());
    assert!(dispatch::find(Stage::Frontend, "view.fit").is_none());
}

#[test]
fn unknown_frontend_command_error() {
    let mut app = crate::EffectcraftApp::new(effectcraft_engine::Session::default());
    let ctx = egui::Context::default();
    assert_eq!(super::frontend(&mut app, &ctx, "nope.nothing", serde_json::json!({})), Err("`nope.nothing` is not a frontend command".to_string()));
    // A prefix handler owns its whole namespace, as before.
    assert_eq!(super::invoke(&mut app, &ctx, "tool.nothing", serde_json::json!({})), Err("unknown tool `nothing`".to_string()));
}
