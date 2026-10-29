#![cfg(all(test, not(target_arch = "wasm32")))]
//! Popups opened from a card on a host's scaled canvas (FigCraft): drawn with the canvas zoom,
//! kept readable (0.75–1.5×), on their button at every zoom; their rows check what's on and
//! hover blue, never in the host's selection colour.

use egui::emath::TSTransform;
use egui::{Context, Event, Id, LayerId, Order, Pos2, Rect, pos2, vec2};

use crate::layer_pointer::{POPUP_SCALE, popup_scale};
use crate::widgets;

const CARD: &str = "test-card-layer";
const POP: &str = "test-guides-pop";

/// The control's rect in the card's layer.
fn button() -> Rect {
    Rect::from_min_size(pos2(40.0, 30.0), vec2(22.0, 22.0))
}

/// One frame of a card layer drawn at `zoom` (window points per layer point, its origin at
/// 100,80) with the Guides menu below its button (`open`: opened this frame). Returns where the
/// list shows in the window (None: closed) and where the button does.
fn frame(ctx: &Context, zoom: f32, events: Vec<Event>, open: bool) -> (Option<Rect>, Rect) {
    let layer = LayerId::new(Order::Middle, Id::new(CARD));
    let xf = TSTransform::new(vec2(100.0, 80.0), zoom);
    let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))), events, ..Default::default() };
    let mut drawn = false;
    let _ = ctx.run_ui(input, |ui| {
        ui.ctx().set_transform_layer(layer, xf);
        egui::Area::new(Id::new(CARD)).order(Order::Middle).fixed_pos(Pos2::ZERO).constrain(false).show(ui.ctx(), |ui| {
            let pid = Id::new(POP);
            if open {
                widgets::open_popup(ui, pid);
            }
            drawn = widgets::popup_is_open(ui, pid);
            let items = ["Title/Action Safe", "Proportional Grid", "Grid", "Guides", "Rulers"];
            let _ = widgets::popup_list(ui, pid, button().left_bottom(), button(), items.len(), 0, |ui| {
                let mut chosen = None;
                for (i, l) in items.iter().enumerate() {
                    if widgets::menu_check(ui, i == 3, l, 170.0).clicked() {
                        chosen = Some(i);
                    }
                }
                chosen
            });
        });
    });
    let area = Id::new(POP).with("area");
    let shown = ctx.memory(|m| m.area_rect(area)).filter(|_| drawn).map(|r| match ctx.layer_transform_to_global(LayerId::new(Order::Foreground, area)) {
        Some(t) => t * r,
        None => r,
    });
    (shown, xf * button())
}

fn is_open(ctx: &Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(Id::new(POP).with("open"))).unwrap_or(false)
}

/// The menu opened and drawn a few frames at `zoom` (an area settles its size over two).
fn settled(ctx: &Context, zoom: f32) -> (Rect, Rect) {
    frame(ctx, zoom, vec![], true);
    let mut out = (None, Rect::NOTHING);
    for _ in 0..3 {
        out = frame(ctx, zoom, vec![], false);
    }
    (out.0.expect("the list shows"), out.1)
}

fn ctx() -> Context {
    let ctx = Context::default();
    crate::theme::install_panel_fonts(&ctx);
    ctx
}

#[test]
fn the_popup_scale_follows_the_canvas_and_stays_readable() {
    assert_eq!(popup_scale(1.0), 1.0);
    assert_eq!(popup_scale(1.2), 1.2);
    assert_eq!(popup_scale(0.25), POPUP_SCALE.0, "23 %: never smaller than 0.75×");
    assert_eq!(popup_scale(4.0), POPUP_SCALE.1, "400 %: never larger than 1.5×");
    for junk in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(popup_scale(junk), 1.0, "{junk}");
    }
}

/// The Guides menu at 25 % and 120 % canvas zoom: 0.75× and 1.2× its size at 100 %, its top left
/// on its button's bottom left, in the window.
#[test]
fn a_cards_popup_scales_with_the_canvas_and_sits_on_its_button() {
    // Its size at 100 % (a layer at 1:1).
    let (base, _) = settled(&ctx(), 1.0);
    for (zoom, k) in [(0.25_f32, 0.75_f32), (1.2, 1.2)] {
        let (shown, button) = settled(&ctx(), zoom);
        assert!((shown.width() - base.width() * k).abs() < 1.0, "at {zoom}: width {} vs {} × {k}", shown.width(), base.width());
        assert!((shown.height() - base.height() * k).abs() < 1.0, "at {zoom}: height {} vs {} × {k}", shown.height(), base.height());
        assert!((shown.min.x - button.min.x).abs() < 0.5, "at {zoom}: left {} on the button's {}", shown.min.x, button.min.x);
        assert!((shown.min.y - button.max.y).abs() < 0.5, "at {zoom}: top {} on the button's bottom {}", shown.min.y, button.max.y);
    }
}

/// The canvas zooming with the menu open: it stays on its button every frame, and stays open
/// (as FigCraft's own menus stay open while the canvas zooms).
#[test]
fn a_cards_popup_stays_on_its_button_while_the_canvas_zooms() {
    let ctx = ctx();
    settled(&ctx, 0.23);
    for zoom in [0.23_f32, 0.4, 0.77, 1.0, 1.2, 0.5] {
        let (shown, button) = frame(&ctx, zoom, vec![], false);
        let shown = shown.expect("still open");
        assert!((shown.min.x - button.min.x).abs() < 0.5 && (shown.min.y - button.max.y).abs() < 0.5, "at {zoom}: {shown:?} vs {button:?}");
    }
    assert!(is_open(&ctx));
}

/// A press is judged where the scaled list shows: one inside it (beyond its unscaled size) keeps
/// it open, one far outside closes it.
#[test]
fn a_press_is_judged_where_the_scaled_list_shows() {
    let press = |at: Pos2, pressed: bool| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    let ctx = ctx();
    let (shown, _) = settled(&ctx, 1.5);
    // The bottom right corner of the shown list, past its unscaled rect, on no row.
    let inside = shown.max - vec2(3.0, 3.0);
    frame(&ctx, 1.5, vec![Event::PointerMoved(inside), press(inside, true)], false);
    frame(&ctx, 1.5, vec![press(inside, false)], false);
    assert!(is_open(&ctx), "a press inside keeps it");
    let far = pos2(1500.0, 950.0);
    frame(&ctx, 1.5, vec![Event::PointerMoved(far), press(far, true)], false);
    assert!(!is_open(&ctx), "a press far outside closes it");
}

/// The rows never take the host's selection colour: a turned-on row paints no fill until
/// hovered, and the hover fill is EffectCraft's blue.
#[test]
fn menu_rows_check_and_hover_blue_whatever_the_hosts_selection_colour() {
    let ctx = ctx();
    // A host whose selection is green (FigCraft's).
    let green = egui::Color32::from_rgb(0x2e, 0xc4, 0x9b);
    ctx.all_styles_mut(|s| {
        s.visuals.selection.bg_fill = green;
        s.visuals.widgets.hovered.weak_bg_fill = green;
    });
    let row = Rect::from_min_size(pos2(0.0, 0.0), vec2(170.0, 22.0));
    let fills = |hover: Option<Pos2>| {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
            events: hover.map(Event::PointerMoved).into_iter().collect(),
            ..Default::default()
        };
        let mut out = vec![];
        for _ in 0..2 {
            let o = ctx.run_ui(input.clone(), |ui| {
                let _ = widgets::menu_check(ui, true, "Guides", row.width());
            });
            out = o
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Rect(r) if r.fill != egui::Color32::TRANSPARENT => Some(r.fill),
                    _ => None,
                })
                .collect::<Vec<_>>();
        }
        out
    };
    let idle = fills(None);
    assert!(!idle.contains(&green), "no green pill on a turned-on row: {idle:?}");
    let hot = fills(Some(pos2(60.0, 11.0)));
    assert!(hot.contains(&widgets::MENU_HOVER), "the row under the pointer is blue: {hot:?}");
    assert!(!hot.contains(&green));
}
