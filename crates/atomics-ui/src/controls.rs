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
        let widgets = &mut ui.style_mut().visuals.widgets;
        for (state, lift) in [(&mut widgets.inactive, 0.0), (&mut widgets.hovered, 0.14), (&mut widgets.active, 0.26)] {
            let fill = lerp(p.accent, p.on_accent, lift);
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
        }
        ui.add(touch::sized(Button::new(label), touch))
    })
    .inner
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
    ui.ctx().animate_bool_with_time(id.with("hover"), hovered, ui.style().animation_time)
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
