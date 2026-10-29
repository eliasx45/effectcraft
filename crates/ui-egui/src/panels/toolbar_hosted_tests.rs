//! EMBED16: the hostable tool slots and viewer bar, drawn for a host with only what
//! `TimelineHost` requires (as FigCraft's card is).

use effectcraft_engine::Session;
use egui::{Event, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
use serde_json::{Value, json};

use super::timeline::{TimelineHost, TimelineParts};
use super::toolbar_hosted::{self as tb, Options};
use crate::automation::Registry;
use crate::state::{Tool, UiState};
use crate::theme::Tokens;

struct Mini {
    session: Session,
    ui: UiState,
    auto: Registry,
    tokens: Tokens,
}

impl TimelineHost for Mini {
    fn session(&self) -> &Session {
        &self.session
    }
    fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }
    fn ui(&self) -> &UiState {
        &self.ui
    }
    fn ui_mut(&mut self) -> &mut UiState {
        &mut self.ui
    }
    fn auto(&self) -> &Registry {
        &self.auto
    }
    fn auto_mut(&mut self) -> &mut Registry {
        &mut self.auto
    }
    fn tokens(&self) -> Tokens {
        self.tokens
    }
    fn parts(&mut self) -> TimelineParts<'_> {
        TimelineParts { session: &mut self.session, ui: &mut self.ui, auto: &mut self.auto, tokens: self.tokens }
    }
    fn invoke(&mut self, _ctx: &egui::Context, id: &str, params: Value) -> Result<Value, String> {
        self.session.execute(id, params).map_err(|e| e.to_string())
    }
}

fn mini() -> Mini {
    let mut session = Session::default();
    session.execute("comp.new", json!({"name": "Card", "width": 320, "height": 180, "duration": 4})).unwrap();
    Mini { session, ui: UiState::default(), auto: Registry::default(), tokens: Tokens::for_kind(crate::theme::ThemeKind::Dark) }
}

fn click(at: Pos2) -> Vec<RawInput> {
    let press = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Default::default() };
    vec![
        RawInput { events: vec![Event::PointerMoved(at)], ..Default::default() },
        RawInput { events: vec![press(true)], ..Default::default() },
        RawInput { events: vec![press(false)], ..Default::default() },
        RawInput::default(),
    ]
}

/// Draw the slots at `side` from the origin for each input, with `works`.
fn run(h: &mut Mini, side: f32, inputs: Vec<RawInput>, works: Option<&dyn Fn(Tool) -> bool>) -> f32 {
    let ctx = egui::Context::default();
    let mut right = 0.0;
    for input in inputs {
        let _ = ctx.run_ui(input, |ui| {
            let opts = Options { side, id_scope: Some(egui::Id::new("card-row")), works, elsewhere: "Use it in EffectCraft." };
            right = tb::slots(h, ui, 0.0, side / 2.0, &opts);
        });
    }
    right
}

/// The host's row is EffectCraft's whole Tools bar: every slot, in order, the same width as the
/// header's at any size, each registered under the header's automation id.
#[test]
fn a_hosts_row_has_every_tool_slot_at_its_own_size() {
    let mut h = mini();
    let right = run(&mut h, 18.0, vec![RawInput::default()], None);
    assert!((right - tb::width(18.0)).abs() < 1e-3, "{right} vs {}", tb::width(18.0));
    assert!((tb::width(26.0) / tb::width(13.0) - 2.0).abs() < 1e-4, "the row scales with its slots");
    for slot in Tool::SLOTS {
        let tool = slot[0];
        assert!(h.auto.elements.iter().any(|e| e.id == format!("tools.{tool:?}")), "{tool:?} is in the row");
    }
}

/// A click on a slot picks its tool on the host's session; a slot the host can't use stays
/// unpicked and says where it works.
#[test]
fn clicks_pick_tools_and_unusable_ones_say_where_they_work() {
    let mut h = mini();
    let side = 18.0;
    let k = side / tb::SIDE;
    // Slot 6 (Rotation): 6 slots of 28, plus the separators after slots 2 and 5.
    let x_of = |slot: usize| (0..slot).map(|s| 28.0 * k + if tb::separator_after(s) { 10.0 * k } else { 0.0 }).sum::<f32>() + side / 2.0;
    run(&mut h, side, click(pos2(x_of(6), side / 2.0)), None);
    assert_eq!(h.ui.tool, Tool::Rotate);
    let works = |t: Tool| t != Tool::Brush;
    run(&mut h, side, click(pos2(x_of(11), side / 2.0)), Some(&works));
    assert_eq!(h.ui.tool, Tool::Rotate, "the Brush isn't picked where it can't work");
    assert!(h.ui.status.contains("Brush") && h.ui.status.contains("EffectCraft"), "{}", h.ui.status);
}

/// The viewer bar draws on a host's session and hands the magnification picked back to the host
/// (it owns the view); junk sizes never panic.
#[test]
fn the_hosted_viewer_bar_draws_on_the_hosts_session() {
    let mut h = mini();
    let ctx = egui::Context::default();
    for zoom in [0.5_f32, f32::NAN, 0.0, 1e9] {
        let _ = ctx.run_ui(RawInput::default(), |ui| {
            let bar = Rect::from_min_size(pos2(0.0, 200.0), vec2(320.0, 30.0));
            let picked = super::viewer_tools::hosted_bar(&mut h, ui, bar, zoom);
            assert!(picked.is_none(), "nothing picked without a click");
        });
    }
    for id in ["viewer.magnification", "viewer.resolution", "viewer.transparency", "viewer.roi", "viewer.grid", "viewer.fastPreviews", "viewer.timecode"] {
        assert!(h.auto.elements.iter().any(|e| e.id == id), "{id} is in the hosted bar");
    }
    assert!(!h.auto.elements.iter().any(|e| e.id == "viewer.previewOn"), "Preview On needs the whole app");
}
