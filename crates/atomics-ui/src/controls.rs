//! What the controls share beyond a plain `ui.button`. The theme gives every control one height, radius and hover
//! ([`Palette::style`](crate::theme::Palette::style)), and a button's own `.fill` or an unselected `.stroke` would
//! switch that hover off. So the primary action and the flat tabs get their look here, and keep it. A view has at most
//! one [`primary`] action.

use egui::{Atom, Button, Color32, Rect, Response, RichText, Stroke, Ui, Vec2, vec2};

use crate::status::{Mark, paint_mark};
use crate::theme::{Palette, lerp};
use crate::touch::{self, Touch};

/// The action a view is there for: filled with the accent, lighter under the pointer.
pub fn primary(ui: &mut Ui, text: impl Into<String>, touch: Option<Touch>, p: &Palette) -> Response {
    let label = RichText::new(text.into()).color(p.on_accent);
    ui.scope(|ui| {
        primary_style(ui, p);
        ui.add(touch::sized(Button::new(label), touch))
    })
    .inner
}

pub(crate) fn primary_style(ui: &mut Ui, p: &Palette) {
    let widgets = &mut ui.style_mut().visuals.widgets;
    for (state, lift) in [(&mut widgets.inactive, 0.0), (&mut widgets.hovered, 0.14), (&mut widgets.active, 0.26)] {
        let fill = lerp(p.accent, p.on_accent, lift);
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
    }
}

/// A tab in a row of them: no frame until the pointer is over it, and outlined while it is the one selected.
pub fn flat<'a>(button: Button<'a>, selected: bool, p: &Palette) -> Button<'a> {
    if selected { button.stroke(Stroke::new(1.0, p.accent)) } else { button.frame_when_inactive(false) }
}

/// An action in a bar (the top bar, a header): its icon and name, with no frame until the pointer is over it, so a
/// row of them reads as calm text and still answers the pointer and the keyboard as any button does.
pub fn bar_button<'a>(icon: &str, label: impl Into<egui::WidgetText>) -> Button<'a> {
    Button::new(crate::icons::with(icon, label)).frame_when_inactive(false)
}

/// A menu that opens from a bar: [`bar_button`]'s look, a submenu with its icon inside another menu.
pub fn bar_menu(
    ui: &mut Ui,
    icon: &str,
    label: impl Into<egui::WidgetText>,
    content: impl FnOnce(&mut Ui),
) -> Response {
    if egui::containers::menu::is_in_menu(ui) {
        return egui::containers::menu::SubMenuButton::new(crate::icons::with(icon, label)).ui(ui, content).0;
    }
    egui::containers::menu::MenuButton::from_button(bar_button(icon, label)).ui(ui, content).0
}

/// How far a control `id` the platform paints itself has come into its hover, from 0 to 1 over the theme's motion
/// time: `hovered` now, or as the last frame left it where the control is drawn before its response is known
/// ([`remember_hover`], [`last_hover`]). Asks for frames only while it changes.
pub fn hover_amount(ui: &Ui, id: egui::Id, hovered: bool) -> f32 {
    ui.ctx().animate_bool_with_time_and_easing(
        id.with("hover"),
        hovered,
        ui.style().animation_time.min(crate::theme::HOVER_TIME),
        egui::emath::easing::quadratic_out,
    )
}

/// How a row, a card or a tab the platform paints itself answers, painted over its `rest` fill and under its content:
/// a wash of the foreground that fades in under the pointer, a stronger one while it is pressed, and a ring while it
/// has the keyboard. Like a button's, the feedback is neutral; the accent keeps to what is selected.
pub fn feedback(ui: &Ui, rect: Rect, response: &Response, radius: impl Into<egui::CornerRadius>, p: &Palette) {
    feedback_where(ui, rect, response, response.hovered(), radius, p);
}

/// [`feedback`] for a row whose own buttons lie over it: `over` says whether the pointer is on the row, them included.
pub fn feedback_where(
    ui: &Ui,
    rect: Rect,
    response: &Response,
    over: bool,
    radius: impl Into<egui::CornerRadius>,
    p: &Palette,
) {
    let radius = radius.into();
    let hover = hover_amount(ui, response.id, over && response.enabled());
    let wash = if response.is_pointer_button_down_on() { PRESSED_WASH } else { HOVER_WASH * hover };
    if wash > 0.0 {
        ui.painter().rect_filled(rect, radius, p.foreground.gamma_multiply(wash));
    }
    if response.has_focus() {
        let ring = Stroke::new(1.5, ui.visuals().selection.stroke.color);
        ui.painter().rect_stroke(rect, radius, ring, egui::StrokeKind::Inside);
    }
}

/// Give native buttons, menu entries and custom click targets the same response, including controls
/// with their own fills. The topmost hit supplies the widget and its paint layer. Only transitioning
/// targets are retained, and no additional input area is registered.
pub(crate) fn install_button_feedback(ctx: &egui::Context) {
    let key = egui::Id::unique("atomics-button-feedback");
    if ctx.data(|d| d.get_temp::<bool>(key)) == Some(true) {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(key, true));
    ctx.on_end_pass(
        "atomics-button-feedback",
        std::sync::Arc::new(|ui| {
            let ctx = ui.ctx();
            let key = egui::Id::unique("animating-buttons");
            let over = ctx.viewport(|v| v.hits.click.as_ref().map(|hit| hit.id));
            let focus = ctx.memory(|m| m.focused());
            let mut targets = ctx.data_mut(|d| d.remove_temp::<Vec<egui::Id>>(key)).unwrap_or_default();
            for id in [over, focus].into_iter().flatten() {
                if !targets.contains(&id) {
                    ctx.animate_bool_with_time(id.with("button-wash"), false, 0.0);
                    targets.push(id);
                }
            }
            targets.retain(|&id| {
                let Some(w) = ctx.viewport(|v| v.this_pass.widgets.get(id).copied()) else {
                    return false;
                };
                if !w.visible || !w.enabled || w.sense != egui::Sense::click() || !w.interact_rect.is_positive() {
                    return false;
                }
                let Some(response) = ctx.read_response(id) else {
                    return false;
                };
                let hovered = over == Some(id) && response.hovered();
                let pressed = response.is_pointer_button_down_on();
                let focused = focus == Some(id);
                let amount = ctx.animate_bool_with_time_and_easing(
                    id.with("button-wash"),
                    hovered || pressed,
                    ui.style().animation_time.min(crate::theme::HOVER_TIME),
                    egui::emath::easing::quadratic_out,
                );
                let amount = if hovered { 0.25 + 0.75 * amount } else { amount };
                if amount <= 0.0 && !focused && !pressed {
                    return false;
                }
                let painter = ctx.layer_painter(w.layer_id).with_clip_rect(w.interact_rect);
                let ink = ui.visuals().strong_text_color();
                let rect = w.rect.shrink(0.75);
                let radius = (rect.height() * 0.28).clamp(2.0, 7.0) as u8;
                let wash = if pressed { 0.14 } else { 0.065 * amount };
                painter.rect_filled(rect, radius, ink.gamma_multiply(wash));
                let strength = if focused {
                    0.65
                } else if pressed {
                    0.55
                } else {
                    0.30 * amount
                };
                painter.rect_stroke(
                    rect,
                    radius,
                    Stroke::new(if focused { 1.5 } else { 1.0 }, ink.gamma_multiply(strength)),
                    egui::StrokeKind::Inside,
                );
                true
            });
            if !targets.is_empty() {
                ctx.data_mut(|d| d.insert_temp(key, targets));
            }
        }),
    );
}

/// An action in words on a row the pointer is over, its right end and its middle at `at`: a small button painted over
/// what the row says there.
pub fn row_action(ui: &Ui, id: egui::Id, at: egui::Pos2, word: &str, p: &Palette) -> Response {
    let text = ui.painter().layout_no_wrap(word.to_string(), p.ui(11.0), p.foreground);
    let size = vec2(text.size().x + 14.0, 20.0);
    let rect = Rect::from_min_size(at - vec2(size.x, size.y / 2.0), size);
    let response = ui.interact(rect, id, egui::Sense::click());
    let hover = hover_amount(ui, id, response.hovered());
    let fill = if response.is_pointer_button_down_on() {
        p.control_pressed()
    } else {
        lerp(p.raised, p.control_hover(), hover)
    };
    let edge = if response.has_focus() { p.accent } else { lerp(p.border, p.control_hover_stroke(), hover) };
    ui.painter().rect(rect, crate::theme::RADIUS, fill, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - text.size() / 2.0, text, p.foreground);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, word));
    response
}

/// How much of the foreground washes over a painted control under the pointer, and while it is pressed.
const HOVER_WASH: f32 = 0.07;
const PRESSED_WASH: f32 = 0.13;

/// A label that acts on a click: in `color`, underlined under the pointer and while it has the keyboard.
pub fn link(ui: &mut Ui, text: impl Into<String>, size: f32, color: Color32) -> Response {
    let text = RichText::new(text.into()).size(size).color(color);
    let response = ui.add(egui::Label::new(text).selectable(false).sense(egui::Sense::click()));
    underline(ui, &response, color);
    response
}

/// The line under a clickable label while the pointer is on it or it has the keyboard.
pub fn underline(ui: &Ui, response: &Response, color: Color32) {
    if response.hovered() || response.has_focus() {
        let y = response.rect.bottom() - 1.0;
        ui.painter().hline(response.rect.x_range(), y, Stroke::new(1.0, color));
    }
}

/// As a frame begins: the pointing hand where a click does something. Whatever draws later in the frame and wants a
/// cursor of its own (a text field, a divider) still gets it, and a surface a click only focuses asks for the plain
/// one ([`surface`]).
pub fn hand(ctx: &egui::Context) {
    let clickable = ctx.viewport(|viewport| {
        viewport.hits.click.as_ref().is_some_and(|widget| widget.enabled && widget.sense == egui::Sense::click())
    });
    if clickable {
        ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

/// A surface whose click only gives it the keyboard (a terminal's body, a panel): the plain cursor over it.
pub fn surface(response: Response) -> Response {
    response.on_hover_cursor(egui::CursorIcon::Default)
}

/// Keeps whether the pointer is over `response`'s control as this frame ends, for [`last_hover`] in the next.
pub fn remember_hover(ui: &Ui, id: egui::Id, response: &Response) {
    let hovered = response.hovered();
    ui.ctx().data_mut(|data| data.insert_temp(id.with("hovered"), hovered));
}

pub fn last_hover(ui: &Ui, id: egui::Id) -> bool {
    ui.ctx().data(|data| data.get_temp(id.with("hovered"))).unwrap_or(false)
}

/// `edit` as tall as the buttons beside it, its text in the middle.
pub fn field<'a>(edit: egui::TextEdit<'a>) -> egui::TextEdit<'a> {
    edit.min_size(vec2(0.0, crate::theme::CONTROL_HEIGHT)).align(egui::Align2::LEFT_CENTER)
}

/// A one-line text field, as tall as the buttons beside it.
pub fn single_line(text: &mut dyn egui::TextBuffer) -> egui::TextEdit<'_> {
    field(egui::TextEdit::singleline(text))
}

/// The down-pointing mark of what opens a menu, in `rect`.
pub fn chevron(painter: &egui::Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let points = vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)];
    painter.add(egui::Shape::line(points, Stroke::new(1.3, color)));
}

/// A dropdown of choices, with the chevron every menu button has.
pub fn combo<'a>(salt: impl std::hash::Hash + std::fmt::Debug) -> egui::ComboBox<'a> {
    egui::ComboBox::from_id_salt(salt).icon(|ui, rect, visuals, _open| {
        chevron(ui.painter(), rect, visuals.fg_stroke.color.gamma_multiply(0.7));
    })
}

/// [`chevron`] as a part of a button.
pub fn chevron_atom<'a>(color: Color32) -> Atom<'a> {
    Atom::paint(vec2(9.0, 9.0), move |ui: &Ui, args: egui::AtomPaintArgs| chevron(ui.painter(), args.rect, color))
}

/// A button that opens a menu and says so with a chevron; inside a menu, a submenu with its own arrow.
pub fn menu_button(ui: &mut Ui, text: impl Into<egui::WidgetText>, content: impl FnOnce(&mut Ui)) -> Response {
    if egui::containers::menu::is_in_menu(ui) {
        return ui.menu_button(text, content).response;
    }
    let arrow = chevron_atom(ui.visuals().weak_text_color());
    egui::containers::menu::MenuButton::from_button(Button::new((text.into(), arrow))).ui(ui, content).0
}

/// A line of text after a painted `mark`, wrapping under itself.
pub fn marked_line(ui: &mut Ui, mark: Mark, color: Color32, text: RichText) {
    ui.horizontal_top(|ui| {
        let height = ui.text_style_height(&egui::TextStyle::Body);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, height), egui::Sense::hover());
        paint_mark(ui.painter(), rect.center(), mark, color);
        ui.add(egui::Label::new(text).wrap());
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomics_core::config::ThemeConfig;

    fn palette() -> Palette {
        let config = ThemeConfig { name: "claude".into(), ..ThemeConfig::default() };
        Palette::new(&atomics_core::theme::resolve(&config, |_| None).tokens, Default::default())
    }

    /// The fills of the rectangles a frame paints, drawing `add` with the pointer at `pointer`, once it has settled.
    fn fills(pointer: Option<egui::Pos2>, p: &Palette, add: impl Fn(&mut Ui) -> Response) -> (Vec<Color32>, Rect) {
        let ctx = egui::Context::default();
        p.apply(&ctx, &crate::theme::tests_support::NoFonts);
        let mut rect = Rect::NOTHING;
        let mut shown = Vec::new();
        for _ in 0..3 {
            let events = pointer.map(egui::Event::PointerMoved).into_iter().collect();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 200.0))),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| rect = add(ui).rect);
            shown = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Rect(painted) if painted.rect.intersects(rect) => Some(painted.fill),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
        }
        (shown, rect)
    }

    #[test]
    fn the_primary_action_is_the_accent_and_answers_the_pointer() {
        let p = palette();
        let draw = |ui: &mut Ui| primary(ui, "Approve plan", None, &p);
        let (rest, rect) = fills(None, &p, draw);
        assert!(rest.contains(&p.accent), "{rest:?}");
        assert_eq!(rect.height(), crate::theme::CONTROL_HEIGHT);
        let (hover, _) = fills(Some(rect.center()), &p, draw);
        assert!(!hover.contains(&p.accent) && hover.contains(&lerp(p.accent, p.on_accent, 0.14)), "{hover:?}");
    }

    /// A built-in theme's palette.
    fn builtin(name: &str) -> Palette {
        let config = ThemeConfig { name: name.into(), ..ThemeConfig::default() };
        Palette::new(&atomics_core::theme::resolve(&config, |_| None).tokens, Default::default())
    }

    /// The WCAG contrast ratio of two opaque colors.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let luminance = |c: Color32| {
            let [r, g, b, _] = egui::Rgba::from(c).to_array();
            0.2126 * r + 0.7152 * g + 0.0722 * b
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn every_built_in_theme_keeps_its_text_readable_and_its_feedback_visible() {
        for name in atomics_core::theme::builtin_names() {
            let p = builtin(name);
            for surface in [p.background, p.panel, p.raised] {
                // WCAG AA for body text; the quiet hints are the one exception, as the theme docs say.
                for (what, text) in [
                    ("foreground", p.foreground),
                    ("muted", p.muted),
                    ("accent", p.accent),
                    ("success", p.success),
                    ("warning", p.warning),
                    ("error", p.error),
                    ("info", p.info),
                ] {
                    let ratio = contrast(text, surface);
                    assert!(ratio >= 4.5, "{name}: {what} on {surface:?} is {ratio:.2}:1");
                }
            }
            assert!(contrast(p.on_accent, p.accent) >= 4.5, "{name}: text on the accent");
            assert!(contrast(p.foreground, p.control_pressed()) >= 4.5, "{name}: text on a pressed control");
            // The pointer and a press each show, without the accent.
            assert!(contrast(p.control_hover(), p.raised) >= 1.15, "{name}: hover");
            assert!(contrast(p.control_pressed(), p.control_hover()) >= 1.1, "{name}: pressed");
        }
    }

    #[test]
    fn a_bar_button_is_calm_at_rest_and_answers_the_pointer_the_press_and_the_keyboard() {
        for name in atomics_core::theme::builtin_names() {
            let p = builtin(name);
            let draw = |ui: &mut Ui| ui.add(bar_button(crate::icons::NEW, "New"));
            let (rest, rect) = fills(None, &p, draw);
            assert!(rest.iter().all(|fill| *fill == Color32::TRANSPARENT), "{name}: {rest:?}");
            let (hover, _) = fills(Some(rect.center()), &p, draw);
            assert!(hover.contains(&p.control_hover()), "{name}: {hover:?}");

            // Pressed: the button held down under the pointer.
            let ctx = egui::Context::default();
            p.apply(&ctx, &crate::theme::tests_support::NoFonts);
            let mut pressed = Vec::new();
            for frame in 0..3 {
                let mut events = vec![egui::Event::PointerMoved(rect.center())];
                if frame == 1 {
                    events.push(egui::Event::PointerButton {
                        pos: rect.center(),
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 200.0))),
                    events,
                    ..Default::default()
                };
                let output = ctx.run_ui(input, |ui| {
                    draw(ui);
                });
                pressed = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Rect(painted) if painted.rect.intersects(rect) => {
                            Some((painted.fill, painted.stroke.color))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
            }
            assert!(pressed.contains(&(p.control_pressed(), p.accent)), "{name}: {pressed:?}");
        }
    }

    /// What a frame paints of a clickable row the platform draws itself, with the pointer at `pointer`, settled; and
    /// the cursor it shows.
    fn row_frame(
        p: &Palette,
        pointer: egui::Pos2,
        pressed: bool,
        sense: egui::Sense,
    ) -> (Vec<Color32>, egui::CursorIcon) {
        let ctx = egui::Context::default();
        p.apply(&ctx, &crate::theme::tests_support::NoFonts);
        let rect = Rect::from_min_size(egui::pos2(10.0, 10.0), vec2(200.0, 30.0));
        let (mut painted, mut cursor) = (Vec::new(), egui::CursorIcon::Default);
        for frame in 0..4 {
            let mut events = vec![egui::Event::PointerMoved(pointer)];
            if pressed && frame == 1 {
                events.push(egui::Event::PointerButton {
                    pos: pointer,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 200.0))),
                // Apart enough to settle, close enough that a held press is still a click.
                time: Some(f64::from(frame) * 0.2),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                hand(ui.ctx());
                let response = ui.interact(rect, egui::Id::unique("row"), sense);
                feedback(ui, rect, &response, 6.0, p);
            });
            painted = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Rect(painted) if painted.rect == rect => Some(painted.fill),
                    _ => None,
                })
                .collect();
            cursor = output.platform_output.cursor_icon;
            output.drop_without_applying_deltas();
        }
        (painted, cursor)
    }

    #[test]
    fn a_painted_row_answers_the_pointer_and_the_press_and_shows_the_hand() {
        for name in atomics_core::theme::builtin_names() {
            let p = builtin(name);
            let (over, away) = (egui::pos2(50.0, 25.0), egui::pos2(300.0, 150.0));
            let click = egui::Sense::click();
            assert_eq!(row_frame(&p, away, false, click), (vec![], egui::CursorIcon::Default), "{name}");
            let (hover, cursor) = row_frame(&p, over, false, click);
            assert_eq!(hover, [p.foreground.gamma_multiply(HOVER_WASH)], "{name}");
            assert_eq!(cursor, egui::CursorIcon::PointingHand, "{name}");
            let (pressed, _) = row_frame(&p, over, true, click);
            assert_eq!(pressed, [p.foreground.gamma_multiply(PRESSED_WASH)], "{name}");
            // What a click only drags or does nothing to keeps the plain cursor.
            for sense in [egui::Sense::hover(), egui::Sense::drag(), egui::Sense::click_and_drag()] {
                assert_eq!(row_frame(&p, over, false, sense).1, egui::CursorIcon::Default, "{name}: {sense:?}");
            }
        }
    }

    #[test]
    fn a_flat_tab_has_no_frame_until_the_pointer_is_over_it() {
        let p = palette();
        let draw = |ui: &mut Ui| ui.add(flat(Button::new("All · 1"), false, &p));
        let (rest, rect) = fills(None, &p, draw);
        assert!(rest.iter().all(|fill| *fill == Color32::TRANSPARENT), "{rest:?}");
        assert_eq!(rect.height(), crate::theme::CONTROL_HEIGHT);
        let (hover, _) = fills(Some(rect.center()), &p, draw);
        assert!(hover.iter().any(|fill| *fill != Color32::TRANSPARENT), "the theme's hover shows: {hover:?}");
    }
}
