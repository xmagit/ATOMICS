//! The UI's look: a resolved theme as egui colors and fonts ([`Palette`]) and the egui style made from them.
//! The host resolves the theme (built-ins, theme files, `[theme]`) into tokens and names its fonts; finding a
//! named font is the host's too, through a [`FontHook`].

mod fonts;

use atomics_core::theme::{ThemeMode, ThemeTokens};
use egui::{Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

pub use fonts::{FontHook, FontSource, Fonts, SIMPLE_FAMILY, definitions, simple_family, simple_font_family};

/// A theme resolved into colors and fonts: a built-in or theme file plus `[theme]` overrides.
pub struct Palette {
    pub mode: ThemeMode,
    /// The main area behind terminals and the diff.
    pub background: Color32,
    pub panel: Color32,
    /// Headers and other surfaces that sit above panels.
    pub raised: Color32,
    pub sidebar: Color32,
    pub topbar: Color32,
    /// Terminal background, also the answer to OSC 11 queries.
    pub terminal: Color32,
    /// Windows, menus and popups.
    pub overlay: Color32,
    pub foreground: Color32,
    pub muted: Color32,
    /// Hints, line numbers and the least important text.
    pub quiet: Color32,
    /// Outlines of buttons, fields and cards.
    pub border: Color32,
    /// Lines between panels and sections.
    pub separator: Color32,
    pub accent: Color32,
    /// Text and icons on accent fills.
    pub on_accent: Color32,
    pub selection: Color32,
    pub cursor: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub error: Color32,
    pub info: Color32,
    pub added: Color32,
    pub removed: Color32,
    pub added_emphasis: Color32,
    pub removed_emphasis: Color32,
    pub ansi: [Color32; 16],
    pub mono_ui: bool,
    pub fonts: Fonts,
}

#[cfg(test)]
const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The height every button, tab and menu button has, whatever the theme's font, and their corners' radius.
pub const CONTROL_HEIGHT: f32 = 22.0;
pub const RADIUS: u8 = 4;

/// The UI text size the built-in themes are drawn at; `ui_size` scales the UI from it. The rest of the type scale:
/// 10.5 for headings in capitals and the smallest notes, 11 for secondary text, 12 for small body text; names and
/// titles above that.
pub const UI_SIZE: f32 = 13.0;

impl Palette {
    /// A resolved theme's tokens in egui colors, with the fonts the host found it naming.
    pub fn new(tokens: &ThemeTokens, fonts: Fonts) -> Palette {
        let t = tokens;
        // Resolution sets every color; a missing one would only come from a broken built-in.
        let c = |value: &Option<String>| value.as_deref().and_then(parse_hex).unwrap_or(Color32::TRANSPARENT);
        let mut ansi = [Color32::TRANSPARENT; 16];
        for (slot, color) in ansi.iter_mut().zip(t.ansi.iter().flatten()) {
            *slot = parse_hex(color).unwrap_or(Color32::TRANSPARENT);
        }
        Palette {
            mode: t.mode.unwrap_or(ThemeMode::Dark),
            background: c(&t.background),
            panel: c(&t.panel),
            raised: c(&t.raised),
            sidebar: c(&t.sidebar),
            topbar: c(&t.topbar),
            terminal: c(&t.terminal),
            overlay: c(&t.overlay),
            foreground: c(&t.foreground),
            muted: c(&t.muted),
            quiet: c(&t.quiet),
            border: c(&t.border),
            separator: c(&t.separator),
            accent: c(&t.accent),
            on_accent: c(&t.on_accent),
            selection: c(&t.selection),
            cursor: c(&t.cursor),
            success: c(&t.success),
            warning: c(&t.warning),
            error: c(&t.error),
            info: c(&t.info),
            added: c(&t.added),
            removed: c(&t.removed),
            added_emphasis: c(&t.added_emphasis),
            removed_emphasis: c(&t.removed_emphasis),
            ansi,
            mono_ui: t.mono_ui.unwrap_or(false),
            fonts,
        }
    }

    /// The xterm 256-color palette with the theme's first 16.
    pub fn indexed(&self, index: usize) -> Color32 {
        match index {
            0..16 => self.ansi[index],
            _ => {
                atomics_core::theme::xterm_color(index).map_or(self.foreground, |[r, g, b]| Color32::from_rgb(r, g, b))
            }
        }
    }

    /// The font for UI text: monospace when the theme asks for it.
    pub fn ui(&self, size: f32) -> egui::FontId {
        if self.mono_ui { egui::FontId::monospace(size * 0.92) } else { egui::FontId::proportional(size) }
    }

    /// A faint wash of the foreground over whatever is under the pointer, in light and dark themes.
    pub fn hover(&self) -> Color32 {
        self.foreground.gamma_multiply(0.04)
    }

    /// Terminal text faded toward the terminal background, as the terminal painter fades it.
    pub fn dim(&self, color: Color32) -> Color32 {
        crate::terminal::dim(self.terminal, color)
    }

    /// How much `ui_size` scales the UI; egui zooms by it.
    pub fn ui_scale(&self) -> f32 {
        self.fonts.ui_size.map_or(1.0, |size| size / UI_SIZE)
    }

    /// A monospace font of `size` points on screen, whatever the UI scale.
    pub fn mono(&self, size: f32) -> egui::FontId {
        egui::FontId::monospace(size / self.ui_scale())
    }

    /// egui's style for this theme: base widgets follow the mode, and every color comes from a token.
    pub fn style(&self) -> egui::Style {
        let p = self;
        let mut style = egui::Style::default();
        let mut visuals = match p.mode {
            ThemeMode::Dark => egui::Visuals::dark(),
            ThemeMode::Light => egui::Visuals::light(),
        };
        visuals.panel_fill = p.panel;
        visuals.window_fill = p.overlay;
        visuals.window_stroke = Stroke::new(1.0, p.border);
        visuals.window_corner_radius = egui::CornerRadius::same(6);
        visuals.menu_corner_radius = egui::CornerRadius::same(6);
        visuals.extreme_bg_color = p.background;
        visuals.text_edit_bg_color = Some(p.background);
        visuals.faint_bg_color = p.raised;
        visuals.code_bg_color = p.raised;
        visuals.weak_text_color = Some(p.quiet);
        visuals.warn_fg_color = p.warning;
        visuals.error_fg_color = p.error;
        visuals.selection.bg_fill = p.selection;
        visuals.selection.stroke = Stroke::new(1.0, p.accent);
        visuals.hyperlink_color = p.accent;
        visuals.text_cursor.stroke = Stroke::new(2.0, p.cursor);
        let widgets = &mut visuals.widgets;
        for state in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            state.corner_radius = egui::CornerRadius::same(RADIUS);
            state.expansion = 0.0;
            state.fg_stroke = Stroke::new(1.0, p.foreground);
        }
        widgets.noninteractive.bg_fill = p.panel;
        widgets.noninteractive.weak_bg_fill = p.panel;
        widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.separator);
        widgets.inactive.bg_fill = p.raised;
        widgets.inactive.weak_bg_fill = p.raised;
        widgets.inactive.bg_stroke = Stroke::new(1.0, p.border);
        widgets.hovered.bg_fill = lerp(p.raised, p.accent, 0.2);
        widgets.hovered.weak_bg_fill = lerp(p.raised, p.accent, 0.2);
        widgets.hovered.bg_stroke = Stroke::new(1.0, lerp(p.border, p.accent, 0.8));
        widgets.active.bg_fill = lerp(p.raised, p.accent, 0.35);
        widgets.active.weak_bg_fill = lerp(p.raised, p.accent, 0.35);
        widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
        // Neutral: egui paints every dialog's title bar with this too, where the accent would say nothing.
        widgets.open.bg_fill = lerp(p.raised, p.foreground, 0.08);
        widgets.open.weak_bg_fill = lerp(p.raised, p.foreground, 0.08);
        widgets.open.bg_stroke = Stroke::new(1.0, p.border);
        style.visuals = visuals;
        if p.mono_ui {
            for font in style.text_styles.values_mut() {
                font.family = egui::FontFamily::Monospace;
                font.size = (font.size * 0.92).round();
            }
        }
        style.spacing.button_padding = vec2(7.0, 2.0);
        style.spacing.interact_size.y = CONTROL_HEIGHT;
        style.spacing.item_spacing = vec2(7.0, 4.0);
        style
    }

    /// A small ATOMICS painted in this theme: the bars, agent rows, a terminal with the ANSI
    /// colors, a diff, the status colors and an accent button. Fonts and sizes are the current ones.
    pub fn show_preview(&self, ui: &mut egui::Ui) -> egui::Response {
        let p = self;
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width().min(680.0), 250.0), Sense::hover());
        let painter = ui.painter_at(rect);
        let ui_font = |size: f32| p.ui(size);
        let mono = FontId::monospace(11.0);
        let text = |at: egui::Pos2, value: &str, font: FontId, color: Color32| {
            painter.text(at, Align2::LEFT_CENTER, value, font, color).width()
        };
        painter.rect_filled(rect, 4.0, p.background);

        let top = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 26.0));
        painter.rect_filled(top, 0.0, p.topbar);
        painter.hline(top.x_range(), top.bottom(), Stroke::new(1.0, p.separator));
        let x = rect.left() + 10.0 + text(pos2(rect.left() + 10.0, top.center().y), "ATOMICS", ui_font(12.5), p.accent);
        text(pos2(x + 8.0, top.center().y), "3 agents", ui_font(11.5), p.muted);
        let settings = Rect::from_center_size(pos2(top.right() - 40.0, top.center().y), vec2(62.0, 18.0));
        painter.rect(settings, 2.0, p.raised, Stroke::new(1.0, p.border), StrokeKind::Inside);
        painter.text(settings.center(), Align2::CENTER_CENTER, "Settings", ui_font(11.0), p.foreground);

        let bottom = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 28.0), rect.max);
        let side = Rect::from_min_max(pos2(rect.left(), top.bottom()), pos2(rect.left() + 150.0, bottom.top()));
        painter.rect_filled(side, 0.0, p.sidebar);
        painter.vline(side.right(), side.y_range(), Stroke::new(1.0, p.separator));
        painter.rect_filled(Rect::from_min_size(side.min + vec2(10.0, 10.0), vec2(3.0, 10.0)), 0.0, p.accent);
        text(pos2(side.left() + 18.0, side.top() + 15.0), "AGENTS", ui_font(10.0), p.muted);
        let rows = [
            ("orbit-tilt", "working", p.accent, true),
            ("readme", "needs you", p.warning, false),
            ("moons", "done", p.success, false),
            ("docs", "exited", p.error, false),
        ];
        for (i, (title, status, color, active)) in rows.into_iter().enumerate() {
            let row =
                Rect::from_min_size(pos2(side.left() + 6.0, side.top() + 28.0 + i as f32 * 34.0), vec2(138.0, 32.0));
            if active {
                painter.rect_filled(row, 2.0, lerp(p.sidebar, p.accent, 0.14));
            }
            painter.circle_filled(pos2(row.left() + 8.0, row.top() + 10.0), 3.0, color);
            let name = if active { p.accent } else { p.foreground };
            text(pos2(row.left() + 16.0, row.top() + 10.0), title, ui_font(11.5), name);
            text(pos2(row.left() + 16.0, row.top() + 24.0), &format!("shell · {status}"), ui_font(10.0), p.quiet);
        }

        let main = Rect::from_min_max(
            pos2(side.right() + 6.0, top.bottom() + 6.0),
            pos2(rect.right() - 6.0, bottom.top() - 6.0),
        );
        let split = main.left() + (main.width() * 0.48).round();
        let tile = Rect::from_min_max(main.min, pos2(split - 3.0, main.bottom()));
        let header = Rect::from_min_max(tile.min, pos2(tile.right(), tile.top() + 20.0));
        painter.rect_filled(tile, 2.0, p.terminal);
        painter.rect_filled(header, 2.0, p.raised);
        painter.rect_stroke(tile, 2.0, Stroke::new(1.0, lerp(p.border, p.accent, 0.7)), StrokeKind::Inside);
        text(pos2(header.left() + 8.0, header.center().y), "orbit-tilt", ui_font(11.0), p.accent);
        let line = |row: usize| tile.top() + 32.0 + row as f32 * 15.0;
        let x = tile.left() + 8.0;
        let w = x + text(pos2(x, line(0)), "$ cargo test ", mono.clone(), p.foreground);
        painter.rect_filled(Rect::from_min_size(pos2(w, line(0) - 7.0), vec2(7.0, 14.0)), 0.0, p.cursor);
        let selected = Rect::from_min_size(pos2(x + 1.0, line(1) - 7.5), vec2(70.0, 15.0));
        painter.rect_filled(selected, 0.0, p.selection);
        text(pos2(x, line(1)), "Compiling orbit v0.1.0", mono.clone(), p.ansi[2]);
        text(pos2(x, line(2)), "warning: unused import", mono.clone(), p.ansi[3]);
        text(pos2(x, line(3)), "error[E0308]: mismatched", mono.clone(), p.ansi[1]);
        let after = x + text(pos2(x, line(4)), "test ", mono.clone(), p.foreground);
        let after = after + text(pos2(after, line(4)), "height ", mono.clone(), p.ansi[4]);
        text(pos2(after, line(4)), "... ok", mono.clone(), p.ansi[6]);
        text(pos2(x, line(5)), "dim · muted output", mono.clone(), p.dim(p.foreground));
        for (i, color) in p.ansi.iter().enumerate() {
            let swatch =
                Rect::from_min_size(pos2(x + (i % 8) as f32 * 16.0, line(6) + (i / 8) as f32 * 14.0), vec2(14.0, 12.0));
            painter.rect_filled(swatch, 1.0, *color);
        }

        let diff = Rect::from_min_max(pos2(split + 3.0, main.top()), main.max);
        painter.rect_filled(diff, 2.0, p.background);
        painter.rect_stroke(diff, 2.0, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let row = |i: usize| {
            Rect::from_min_size(
                pos2(diff.left() + 1.0, diff.top() + 8.0 + i as f32 * 16.0),
                vec2(diff.width() - 2.0, 16.0),
            )
        };
        text(pos2(diff.left() + 8.0, row(0).center().y), "M src/lib.rs  +2 −1", ui_font(11.0), p.foreground);
        text(pos2(diff.left() + 40.0, row(1).center().y), "@@ -3,4 +3,5 @@", mono.clone(), p.info);
        let lines = [
            (None, "3", "/// A body circling a planet.", p.foreground),
            (Some((p.removed, p.removed_emphasis)), "4", "pub fn speed() -> f64 {", p.foreground),
            (Some((p.added, p.added_emphasis)), "4", "pub fn height() -> f64 {", p.foreground),
            (Some((p.added, p.added_emphasis)), "5", "    self.tilt.sin()", p.foreground),
            (None, "6", "}", p.foreground),
        ];
        for (i, (fill, number, code, color)) in lines.into_iter().enumerate() {
            let line = row(i + 2);
            if let Some((fill, emphasis)) = fill {
                painter.rect_filled(line, 0.0, fill);
                painter.rect_filled(
                    Rect::from_min_size(pos2(line.left() + 75.0, line.top()), vec2(42.0, 16.0)),
                    0.0,
                    emphasis,
                );
            }
            text(pos2(line.left() + 8.0, line.center().y), number, mono.clone(), p.quiet);
            text(pos2(line.left() + 40.0, line.center().y), code, mono.clone(), color);
        }
        let popup = Rect::from_min_size(pos2(diff.right() - 120.0, diff.bottom() - 58.0), vec2(112.0, 50.0));
        painter.rect(popup, 4.0, p.overlay, Stroke::new(1.0, p.border), StrokeKind::Inside);
        text(pos2(popup.left() + 8.0, popup.top() + 13.0), "Open in editor", ui_font(11.0), p.foreground);
        painter.rect_filled(
            Rect::from_min_size(pos2(popup.left() + 4.0, popup.top() + 26.0), vec2(104.0, 18.0)),
            2.0,
            lerp(p.overlay, p.accent, 0.2),
        );
        text(pos2(popup.left() + 8.0, popup.top() + 35.0), "Copy path", ui_font(11.0), p.foreground);

        painter.rect_filled(bottom, 0.0, p.panel);
        painter.hline(bottom.x_range(), bottom.top(), Stroke::new(1.0, p.separator));
        let field = Rect::from_min_max(
            pos2(bottom.left() + 10.0, bottom.top() + 5.0),
            pos2(bottom.right() - 290.0, bottom.bottom() - 5.0),
        );
        painter.rect(field, 2.0, p.background, Stroke::new(1.0, p.border), StrokeKind::Inside);
        text(pos2(field.left() + 6.0, field.center().y), "Steer · Enter sends", ui_font(11.0), p.quiet);
        let mut x = field.right() + 10.0;
        for (label, color) in [("done", p.success), ("needs you", p.warning), ("failed", p.error), ("report", p.info)] {
            x += text(pos2(x, bottom.center().y), label, ui_font(11.0), color) + 9.0;
        }
        let send = Rect::from_min_size(pos2(bottom.right() - 52.0, bottom.top() + 5.0), vec2(42.0, 18.0));
        painter.rect_filled(send, 2.0, p.accent);
        painter.text(send.center(), Align2::CENTER_CENTER, "Send", ui_font(11.0), p.on_accent);
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0, p.border), StrokeKind::Inside);
        response
    }

    /// Applies the theme to egui: one style whatever the system's appearance, UI scale, and the fonts,
    /// which `fonts` supplies.
    pub fn apply(&self, ctx: &egui::Context, fonts: &dyn FontHook) {
        let style = std::sync::Arc::new(self.style());
        ctx.set_style_of(egui::Theme::Dark, style.clone());
        ctx.set_style_of(egui::Theme::Light, style);
        ctx.set_theme(match self.mode {
            ThemeMode::Dark => egui::Theme::Dark,
            ThemeMode::Light => egui::Theme::Light,
        });
        // Only a change of `ui_size` resets the zoom, so keyboard zoom survives reloads.
        let key = egui::Id::unique("atomics-theme-ui-scale");
        let scale = self.ui_scale();
        if ctx.data(|d| d.get_temp::<f32>(key)).unwrap_or(1.0) != scale {
            ctx.set_zoom_factor(scale);
        }
        ctx.data_mut(|d| d.insert_temp(key, scale));
        fonts.apply(ctx, &self.fonts);
    }
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

pub fn parse_hex(text: &str) -> Option<Color32> {
    atomics_core::theme::parse_color(text).map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

/// What a test of the look needs of a theme.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// Fonts the host never found; the style and scale tests need none.
    pub struct NoFonts;

    impl FontHook for NoFonts {
        fn apply(&self, _: &egui::Context, _: &Fonts) {}
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::NoFonts;
    use super::*;
    use atomics_core::config::ThemeConfig;

    fn builtin(name: &str) -> Palette {
        let config = ThemeConfig { name: name.into(), ..ThemeConfig::default() };
        Palette::new(&atomics_core::theme::resolve(&config, |_| None).tokens, Fonts::default())
    }

    #[test]
    fn hex_and_palette() {
        assert_eq!(parse_hex("#7aa2f7"), Some(Color32::from_rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(parse_hex("nope"), None);
        let palette = builtin("claude");
        assert_eq!(palette.accent, rgb(0xd77757));
        assert!(palette.mono_ui);
        assert_eq!(palette.indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(palette.indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(palette.indexed(232), Color32::from_rgb(8, 8, 8));
        assert_eq!(palette.indexed(3), palette.ansi[3]);
        assert_eq!(palette.indexed(256), palette.foreground);
    }

    #[test]
    fn light_themes_reach_egui_whatever_the_system_appearance() {
        let paper = builtin("paper");
        assert_eq!(paper.mode, ThemeMode::Light);
        let ctx = egui::Context::default();
        for (palette, system, dark) in
            [(&paper, egui::Theme::Dark, false), (&builtin("claude"), egui::Theme::Light, true)]
        {
            palette.apply(&ctx, &NoFonts);
            let output = ctx.run_ui(egui::RawInput { system_theme: Some(system), ..Default::default() }, |_| {});
            output.drop_without_applying_deltas();
            let visuals = ctx.global_style().visuals.clone();
            assert_eq!(visuals.dark_mode, dark, "the theme's mode wins over a {system:?} system");
            assert_eq!(visuals.panel_fill, palette.panel);
            assert_eq!(visuals.window_fill, palette.overlay);
            assert_eq!(visuals.widgets.inactive.bg_fill, palette.raised);
            // What egui fills a dialog's title bar with: no tint of the accent.
            assert_eq!(visuals.widgets.open.weak_bg_fill, lerp(palette.raised, palette.foreground, 0.08));
            assert_eq!(visuals.widgets.inactive.fg_stroke.color, palette.foreground);
            assert_eq!(visuals.selection.bg_fill, palette.selection);
            assert_eq!(visuals.text_cursor.stroke.color, palette.cursor);
        }
    }

    #[test]
    fn the_font_hook_gets_the_themes_fonts() {
        struct Seen(std::cell::RefCell<Vec<Fonts>>);
        impl FontHook for Seen {
            fn apply(&self, _: &egui::Context, fonts: &Fonts) {
                self.0.borrow_mut().push(fonts.clone());
            }
        }
        let fonts = Fonts { ui: Some(FontSource::Family("Inter".into())), ui_size: Some(15.6), ..Fonts::default() };
        let palette = Palette::new(&atomics_core::theme::builtin("claude").unwrap(), fonts.clone());
        let seen = Seen(Default::default());
        let ctx = egui::Context::default();
        palette.apply(&ctx, &seen);
        assert_eq!(*seen.0.borrow(), [fonts]);
        assert!((palette.ui_scale() - 1.2).abs() < 1e-6);
        assert!((palette.mono(13.0).size * 1.2 - 13.0).abs() < 1e-4, "terminal text keeps its size under zoom");
        ctx.run_ui(egui::RawInput::default(), |_| {}).drop_without_applying_deltas();
        assert!((ctx.zoom_factor() - 1.2).abs() < 1e-6, "ui_size zooms the UI: {}", ctx.zoom_factor());
    }
}
