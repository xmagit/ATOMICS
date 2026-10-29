mod fonts;

use std::path::Path;

use egui::{Color32, Key, KeyboardShortcut, Modifiers, Stroke, vec2};
use imacs_core::config::{Chord, ThemeConfig};
use imacs_core::theme::{Resolved, ThemeMode};

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

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// The UI text size the built-in themes are drawn at; `ui_size` scales the UI from it.
const UI_SIZE: f32 = 13.0;

impl Palette {
    /// `[theme]` resolved against the built-in themes only.
    pub fn from_config(theme: &ThemeConfig) -> Palette {
        Palette::from_resolved(imacs_core::theme::resolve(theme, |_| None), Path::new(".")).0
    }

    /// `[theme]` resolved against the theme files in `dir`, with what could not be applied.
    pub fn load(theme: &ThemeConfig, dir: &Path) -> (Palette, Vec<String>) {
        Palette::from_resolved(imacs_core::theme::load(theme, dir), dir)
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
        let key = egui::Id::unique("imacs-theme-ui-scale");
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
    imacs_core::theme::parse_color(text).map(|[r, g, b]| Color32::from_rgb(r, g, b))
}

/// The modifier reserved for IMACS's own shortcuts, so it never reaches a terminal:
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
        let dir = tempfile::tempdir().unwrap();
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
}
