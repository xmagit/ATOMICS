mod fonts;

use std::path::Path;

use atomics_core::config::{Chord, ThemeConfig};
use atomics_core::theme::{Resolved, ThemeMode};
use egui::{Align2, Color32, FontId, Key, KeyboardShortcut, Modifiers, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

pub use fonts::{FontSource, Fonts};

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
    /// Syntax theme for the diff and file view.
    pub syntax: String,
    pub mono_ui: bool,
    pub fonts: Fonts,
}

#[cfg(test)]
const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The UI text size the built-in themes are drawn at; `ui_size` scales the UI from it.
const UI_SIZE: f32 = 13.0;

impl Palette {
    /// `[theme]` resolved against the built-in themes only.
    #[cfg(test)]
    pub fn from_config(theme: &ThemeConfig) -> Palette {
        Palette::from_resolved(atomics_core::theme::resolve(theme, |_| None), Path::new(".")).0
    }

    /// `[theme]` resolved against the theme files in `dir`, with what could not be applied.
    pub fn load(theme: &ThemeConfig, dir: &Path) -> (Palette, Vec<String>) {
        Palette::from_resolved(atomics_core::theme::load(theme, dir), dir)
    }

    fn from_resolved(resolved: Resolved, dir: &Path) -> (Palette, Vec<String>) {
        let Resolved { tokens: t, mut errors, .. } = resolved;
        // Resolution sets every color; a missing one would only come from a broken built-in.
        let c = |value: &Option<String>| value.as_deref().and_then(parse_hex).unwrap_or(Color32::TRANSPARENT);
        let mode = t.mode.unwrap_or(ThemeMode::Dark);
        let syntax = match t.syntax {
            Some(name) if crate::highlight::theme_named(&name).is_some() => name,
            Some(name) => {
                errors.push(format!("unknown syntax theme \"{name}\""));
                mode.default_syntax().into()
            }
            None => mode.default_syntax().into(),
        };
        let mut ansi = [Color32::TRANSPARENT; 16];
        for (slot, color) in ansi.iter_mut().zip(t.ansi.iter().flatten()) {
            *slot = parse_hex(color).unwrap_or(Color32::TRANSPARENT);
        }
        let palette = Palette {
            mode,
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
            syntax,
            mono_ui: t.mono_ui.unwrap_or(false),
            fonts: Fonts {
                ui: t.ui_font.as_deref().map(|value| FontSource::parse(value, dir)),
                mono: t.mono_font.as_deref().map(|value| FontSource::parse(value, dir)),
                ui_size: t.ui_size,
                mono_size: t.mono_size,
                diff_size: t.diff_size,
            },
        };
        (palette, errors)
    }

    /// The xterm 256-color palette with the theme's first 16.
    pub fn indexed(&self, index: usize) -> Color32 {
        match index {
            0..16 => self.ansi[index],
            16..232 => {
                let i = index - 16;
                let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
                Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..256 => {
                let v = (8 + (index - 232) * 10) as u8;
                Color32::from_rgb(v, v, v)
            }
            _ => self.foreground,
        }
    }

    /// The font for UI text: monospace when the theme asks for it.
    pub fn ui(&self, size: f32) -> egui::FontId {
        if self.mono_ui { egui::FontId::monospace(size * 0.92) } else { egui::FontId::proportional(size) }
    }

    /// `COLORFGBG` for programs that read it instead of asking the terminal: white on black, or black on white.
    pub fn colorfgbg(&self) -> &'static str {
        match self.mode {
            ThemeMode::Dark => "15;0",
            ThemeMode::Light => "0;15",
        }
    }

    /// A faint wash of the foreground over whatever is under the pointer, in light and dark themes.
    pub fn hover(&self) -> Color32 {
        self.foreground.gamma_multiply(0.04)
    }

    /// Terminal text faded toward the terminal background.
    pub fn dim(&self, color: Color32) -> Color32 {
        lerp(self.terminal, color, 0.62)
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
        visuals.window_corner_radius = egui::CornerRadius::same(4);
        visuals.menu_corner_radius = egui::CornerRadius::same(3);
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
            state.corner_radius = egui::CornerRadius::same(2);
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
        widgets.open.bg_fill = lerp(p.raised, p.accent, 0.25);
        widgets.open.weak_bg_fill = lerp(p.raised, p.accent, 0.25);
        widgets.open.bg_stroke = Stroke::new(1.0, p.border);
        style.visuals = visuals;
        if p.mono_ui {
            for font in style.text_styles.values_mut() {
                font.family = egui::FontFamily::Monospace;
                font.size = (font.size * 0.92).round();
            }
        }
        style.spacing.button_padding = vec2(7.0, 2.0);
        style.spacing.item_spacing = vec2(7.0, 4.0);
        style
    }

    /// What a proposed theme file would look like, resolved against the other theme files in `dir`.
    pub fn preview(name: &str, text: &str, dir: &Path) -> (Palette, Vec<String>) {
        let config = ThemeConfig { name: name.into(), ..ThemeConfig::default() };
        let resolved = atomics_core::theme::resolve(&config, |file| {
            if file == name { Some(Ok(text.to_string())) } else { atomics_core::theme::read_file(dir, file) }
        });
        Palette::from_resolved(resolved, dir)
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

    /// Applies the theme to egui: one style whatever the system's appearance, fonts, and UI scale.
    pub fn apply(&self, ctx: &egui::Context) {
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
        fonts::apply(ctx, &self.fonts);
    }
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

pub fn parse_hex(text: &str) -> Option<Color32> {
    atomics_core::theme::parse_color(text).map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

/// The modifier reserved for ATOMICS's own shortcuts, so it never reaches a terminal:
/// Command on macOS, Ctrl+Shift elsewhere (plain Ctrl belongs to the shell).
pub fn is_app_chord(m: &Modifiers) -> bool {
    if cfg!(target_os = "macos") { m.mac_cmd } else { m.ctrl && m.shift }
}

pub fn shortcut(text: &str) -> Option<KeyboardShortcut> {
    let chord = Chord::parse(text)?;
    let mut chars = chord.key.chars();
    let capitalized: String =
        chars.next().into_iter().flat_map(char::to_uppercase).chain(chars.flat_map(char::to_lowercase)).collect();
    let key = Key::from_name(&chord.key)
        .or_else(|| Key::from_name(&chord.key.to_uppercase()))
        .or_else(|| Key::from_name(&capitalized))?;
    let mut modifiers = Modifiers::NONE;
    if chord.cmd {
        modifiers |= if cfg!(target_os = "macos") { Modifiers::MAC_CMD } else { Modifiers::CTRL | Modifiers::SHIFT };
    }
    if chord.primary {
        modifiers |= if cfg!(target_os = "macos") { Modifiers::MAC_CMD } else { Modifiers::CTRL };
    }
    if chord.ctrl {
        modifiers |= Modifiers::CTRL;
    }
    if chord.alt {
        modifiers |= Modifiers::ALT;
    }
    if chord.shift {
        modifiers |= Modifiers::SHIFT;
    }
    Some(KeyboardShortcut::new(modifiers, key))
}

/// Navigation follows physical keys across layouts; extra modifiers belong to the terminal.
pub fn consume_physical_shortcut(input: &mut egui::InputState, chord: &KeyboardShortcut) -> bool {
    let before = input.events.len();
    input.events.retain(|event| {
        !matches!(event, egui::Event::Key { key, physical_key, modifiers, pressed: true, .. }
            if physical_key.unwrap_or(*key) == chord.logical_key
                && modifiers.matches_exact(chord.modifiers))
    });
    input.events.len() < before
}

/// Consumes a configured chord taken before the terminal, with exactly its modifiers. A letter
/// matches the key the keyboard layout types, as the other `[keys]` bindings do, so Ctrl+G is the
/// key labelled G on Colemak or Dvorak too. A digit matches its physical key, like Ctrl/Command+1…9,
/// because some layouts type digits only with Shift. Layouts without Latin letters still work:
/// egui reports the Latin key at that position as the logical key.
pub fn consume_chord(input: &mut egui::InputState, chord: &KeyboardShortcut) -> bool {
    if is_digit(chord.logical_key) {
        return consume_physical_shortcut(input, chord);
    }
    let before = input.events.len();
    input.events.retain(|event| {
        !matches!(event, egui::Event::Key { key, modifiers, pressed: true, .. }
            if *key == chord.logical_key && modifiers.matches_exact(chord.modifiers))
    });
    input.events.len() < before
}

pub fn is_digit(key: Key) -> bool {
    matches!(
        key,
        Key::Num0
            | Key::Num1
            | Key::Num2
            | Key::Num3
            | Key::Num4
            | Key::Num5
            | Key::Num6
            | Key::Num7
            | Key::Num8
            | Key::Num9
    )
}

/// Display exactly the platform modifiers used to consume the configured chord.
pub fn shortcut_label(ctx: &egui::Context, text: &str) -> String {
    shortcut(text).map(|chord| ctx.format_shortcut(&chord)).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_palette() {
        assert_eq!(parse_hex("#7aa2f7"), Some(Color32::from_rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(parse_hex("nope"), None);
        let palette = Palette::from_config(&ThemeConfig::default());
        assert_eq!(palette.accent, rgb(0xd77757));
        assert_eq!(palette.syntax, "Monokai Extended");
        assert!(palette.mono_ui);
        let mut custom = ThemeConfig::default();
        custom.tokens.cursor = Some("#00ff00".into());
        assert_eq!(Palette::from_config(&custom).cursor, rgb(0x00ff00));
        let midnight = ThemeConfig { name: "midnight".into(), ..ThemeConfig::default() };
        assert_eq!(Palette::from_config(&midnight).syntax, "OneHalfDark");
        assert_eq!(palette.indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(palette.indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(palette.indexed(232), Color32::from_rgb(8, 8, 8));
    }

    #[test]
    fn theme_files_reach_the_palette_and_style() {
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mine.toml"),
            "extends = \"midnight\"\nsidebar = \"#010203\"\noverlay = \"#040506\"\nseparator = \"#070809\"\nquiet = \"#0a0b0c\"\nsyntax = \"no such theme\"\nui_font = \"fonts/ui.ttf\"\nmono_font = \"IBM Plex Mono\"\nui_size = 15.6",
        )
        .unwrap();
        let config = ThemeConfig { name: "mine".into(), ..ThemeConfig::default() };
        let (palette, errors) = Palette::load(&config, dir.path());
        assert_eq!(errors, ["unknown syntax theme \"no such theme\""]);
        assert_eq!(palette.syntax, "OneHalfDark", "an unknown syntax theme falls back to the mode's default");
        assert_eq!(palette.sidebar, rgb(0x010203));
        assert_eq!(palette.fonts.ui, Some(FontSource::File(dir.path().join("fonts/ui.ttf"))));
        assert_eq!(palette.fonts.mono, Some(FontSource::Family("IBM Plex Mono".into())));
        assert!((palette.ui_scale() - 1.2).abs() < 1e-6);
        assert!((palette.mono(13.0).size * 1.2 - 13.0).abs() < 1e-4, "terminal text keeps its size under zoom");
        let style = palette.style();
        assert_eq!(style.visuals.window_fill, rgb(0x040506));
        assert_eq!(style.visuals.widgets.noninteractive.bg_stroke.color, rgb(0x070809));
        assert_eq!(style.visuals.weak_text_color, Some(rgb(0x0a0b0c)));
        assert_eq!(style.visuals.selection.bg_fill, palette.selection);
        assert!(style.visuals.dark_mode);
    }

    #[test]
    fn proposed_themes_preview_with_their_own_colors() {
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::write(dir.path().join("base.toml"), "extends = \"paper\"\nadded = \"#c8f0d0\"").unwrap();
        let text = "extends = \"base\"\nterminal = \"#fffdf6\"\naccent = \"#2a6fdb\"\non_accent = \"#fafafa\"";
        let (palette, errors) = Palette::preview("ocean", text, dir.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(palette.mode, ThemeMode::Light, "the preview follows extends through the other files");
        assert_eq!(palette.added, rgb(0xc8f0d0));
        assert_eq!(palette.accent, rgb(0x2a6fdb));

        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            palette.show_preview(ui);
        });
        let fills: Vec<Color32> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) => Some(rect.fill),
                _ => None,
            })
            .collect();
        for (token, color) in [
            ("terminal", palette.terminal),
            ("accent", palette.accent),
            ("added", palette.added),
            ("selection", palette.selection),
            ("overlay", palette.overlay),
            ("ansi 9", palette.ansi[9]),
        ] {
            assert!(fills.contains(&color), "the preview paints {token}");
        }
        let send = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "Send" => Some(text.clone()),
            _ => None,
        });
        assert_eq!(send.unwrap().fallback_color, palette.on_accent, "Send is written in on_accent");
        output.drop_without_applying_deltas();
    }

    #[test]
    fn light_themes_reach_egui_whatever_the_system_appearance() {
        let paper = Palette::from_config(&ThemeConfig { name: "paper".into(), ..ThemeConfig::default() });
        assert_eq!(paper.mode, ThemeMode::Light);
        assert_eq!(paper.syntax, "OneHalfLight");
        let ctx = egui::Context::default();
        for (palette, system, dark) in [
            (&paper, egui::Theme::Dark, false),
            (&Palette::from_config(&ThemeConfig::default()), egui::Theme::Light, true),
        ] {
            palette.apply(&ctx);
            let output = ctx.run_ui(egui::RawInput { system_theme: Some(system), ..Default::default() }, |_| {});
            output.drop_without_applying_deltas();
            let visuals = ctx.global_style().visuals.clone();
            assert_eq!(visuals.dark_mode, dark, "the theme's mode wins over a {system:?} system");
            assert_eq!(visuals.panel_fill, palette.panel);
            assert_eq!(visuals.window_fill, palette.overlay);
            assert_eq!(visuals.widgets.inactive.bg_fill, palette.raised);
            assert_eq!(visuals.widgets.inactive.fg_stroke.color, palette.foreground);
            assert_eq!(visuals.selection.bg_fill, palette.selection);
            assert_eq!(visuals.text_cursor.stroke.color, palette.cursor);
        }
    }

    /// Colors written into UI code instead of taken from the theme, as `file:line: code`.
    fn color_literals(file: &Path, text: &str) -> Vec<String> {
        let code: Vec<&str> = text.lines().map(|line| line.split("//").next().unwrap_or("")).collect();
        let joined = code.join("\n");
        let numeric = |arg: &str| {
            let arg = arg.trim().replace('_', "");
            let unsuffixed = |text: &str, suffixes: &[&str]| {
                suffixes.iter().find_map(|suffix| text.strip_suffix(suffix)).unwrap_or(text).to_string()
            };
            match arg.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(&unsuffixed(hex, &["u8", "u32", "u64"]), 16).is_ok(),
                None => unsuffixed(&arg, &["u8", "u32", "u64", "f32", "f64"]).parse::<f64>().is_ok(),
            }
        };
        let mut found = Vec::new();
        for ty in ["Color32::", "Rgba::", "Hsva::", "Hsva {", "hex_color!("] {
            for (at, _) in joined.match_indices(ty) {
                let rest = &joined[at + ty.len()..];
                let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                let args = rest[name.len()..].strip_prefix('(').map(|args| {
                    let mut depth = 1;
                    let end = args
                        .char_indices()
                        .find(|&(_, c)| {
                            depth += match c {
                                '(' => 1,
                                ')' => -1,
                                _ => 0,
                            };
                            depth == 0
                        })
                        .map_or(args.len(), |(end, _)| end);
                    &args[..end]
                });
                let literal = match name.as_str() {
                    "TRANSPARENT" => false,
                    constant if constant.chars().next().is_some_and(char::is_uppercase) => true,
                    "from_gray" | "from_white_alpha" | "from_black_alpha" | "additive_luminance" | "from_hex" => true,
                    _ if ty == "Hsva {" || ty == "hex_color!(" => true,
                    _ => args.is_some_and(|args| args.split(',').any(numeric)),
                };
                if literal {
                    let line = joined[..at].matches('\n').count();
                    found.push(format!("{}:{}: {}", file.display(), line + 1, code[line].trim()));
                }
            }
        }
        found
    }

    #[test]
    fn ui_code_takes_every_color_from_the_theme() {
        fn rust_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, files);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        assert!(files.len() > 20, "the scan must see the UI code");
        let found: Vec<String> = files
            .iter()
            .filter(|file| **file != src.join("theme.rs"))
            .flat_map(|file| color_literals(file.strip_prefix(&src).unwrap(), &std::fs::read_to_string(file).unwrap()))
            .collect();
        assert!(found.is_empty(), "use a theme token from theme.rs instead of:\n{}", found.join("\n"));

        // The scan itself catches each way of writing a color.
        let file = Path::new("x.rs");
        for literal in [
            "let c = Color32::WHITE;",
            "egui::Color32::from_rgb(255, 0, 0)",
            "Color32::from_rgba_unmultiplied(\n    0x10,\n    a,\n    b,\n    c,\n)",
            "Color32::from_white_alpha(8)",
            "Color32::from_gray(v)",
            "egui::Rgba::from_rgb(1.0, 0.5, 0.0)",
            "Color32::from_rgb(10u8, g, b)",
            "Color32::from_rgb(0xff, 0x32, 0)",
            "Color32::from_rgb(r, 0x1fu8, b)",
        ] {
            assert_eq!(color_literals(file, literal).len(), 1, "{literal}");
        }
        for allowed in [
            "Color32::TRANSPARENT",
            "Color32::from_rgb(rgb.r, rgb.g, rgb.b)",
            "Color32::from_rgba_unmultiplied(s.color[0], s.color[1], s.color[2], s.color[3])",
            "fn chip(color: Color32) {} // Color32::WHITE in a comment",
        ] {
            assert!(color_literals(file, allowed).is_empty(), "{allowed}");
        }
    }

    #[test]
    fn shortcuts_parse() {
        let s = shortcut("cmd+d").unwrap();
        assert_eq!(s.logical_key, Key::D);
        let voice = shortcut("cmd+shift+space").unwrap();
        assert_eq!(voice.logical_key, Key::Space);
        assert!(voice.modifiers.shift);
        assert_eq!(shortcut("cmd+Escape").unwrap().logical_key, Key::Escape);
        assert!(shortcut("cmd+notakey").is_none());
        let cmd = shortcut("cmd+d").unwrap().modifiers;
        assert!(is_app_chord(&cmd));
        if cfg!(target_os = "macos") {
            assert!(cmd.mac_cmd && !cmd.ctrl && !cmd.shift);
        } else {
            assert!(cmd.ctrl && cmd.shift && !cmd.mac_cmd);
            assert!(!is_app_chord(&Modifiers::CTRL), "plain Ctrl must reach the terminal");
            assert_eq!(shortcut("cmd+shift+space"), shortcut("ctrl+shift+space"));
        }
    }

    #[test]
    fn digit_chords_match_the_physical_key_when_shift_types_a_symbol() {
        let chord = KeyboardShortcut::new(shortcut("cmd+a").unwrap().modifiers, Key::Num1);
        let press = |key, physical_key, modifiers| egui::Event::Key {
            key,
            physical_key: Some(physical_key),
            pressed: true,
            repeat: false,
            modifiers,
        };
        let mut input = egui::InputState::default();
        // Shift+1 arrives as `!` on most layouts; the physical key is still 1.
        input.events = vec![press(Key::Exclamationmark, Key::Num1, chord.modifiers)];
        assert!(consume_physical_shortcut(&mut input, &chord));
        assert!(input.events.is_empty());
        input.events = vec![press(Key::Num1, Key::Num1, chord.modifiers)];
        assert!(consume_physical_shortcut(&mut input, &chord), "an unshifted digit still matches");
        input.events = vec![egui::Event::Key {
            key: Key::Num1,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: chord.modifiers,
        }];
        assert!(consume_physical_shortcut(&mut input, &chord), "fall back when no physical key is supplied");
        for other in [
            press(Key::Num2, Key::Num2, chord.modifiers),
            press(Key::Num1, Key::Num2, chord.modifiers),
            press(Key::Num1, Key::Num1, Modifiers::CTRL),
            press(Key::Num1, Key::Num1, chord.modifiers | Modifiers::ALT),
        ] {
            input.events = vec![other];
            assert!(!consume_physical_shortcut(&mut input, &chord));
            assert_eq!(input.events.len(), 1, "other digits and plain Ctrl stay for the terminal");
        }
    }
}
