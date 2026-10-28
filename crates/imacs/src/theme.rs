use egui::{Color32, Key, KeyboardShortcut, Modifiers};
use imacs_core::config::{Chord, ThemeConfig};

/// Colors for terminals and the diff, from a `[theme]` preset plus overrides.
pub struct Palette {
    pub background: Color32,
    pub panel: Color32,
    /// Headers and other surfaces that sit above panels.
    pub raised: Color32,
    pub border: Color32,
    pub foreground: Color32,
    pub muted: Color32,
    pub cursor: Color32,
    pub selection: Color32,
    pub accent: Color32,
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
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Palette {
    pub fn from_config(theme: &ThemeConfig) -> Palette {
        let mut palette = match theme.preset.to_ascii_lowercase().as_str() {
            "midnight" => Palette::midnight(),
            _ => Palette::claude(),
        };
        let set = |target: &mut Color32, value: &Option<String>| {
            if let Some(color) = value.as_deref().and_then(parse_hex) {
                *target = color;
            }
        };
        set(&mut palette.background, &theme.background);
        set(&mut palette.panel, &theme.panel);
        set(&mut palette.raised, &theme.raised);
        set(&mut palette.border, &theme.border);
        set(&mut palette.foreground, &theme.foreground);
        set(&mut palette.muted, &theme.muted);
        set(&mut palette.accent, &theme.accent);
        set(&mut palette.added, &theme.added);
        set(&mut palette.removed, &theme.removed);
        set(&mut palette.added_emphasis, &theme.added_emphasis);
        set(&mut palette.removed_emphasis, &theme.removed_emphasis);
        if theme.accent.is_some() {
            palette.cursor = palette.accent;
        }
        if let Some(syntax) = &theme.syntax {
            palette.syntax = syntax.clone();
        }
        if let Some(mono) = theme.mono_ui {
            palette.mono_ui = mono;
        }
        palette
    }

    /// Near-black with Claude Code's orange and its diff colors.
    fn claude() -> Palette {
        let accent = rgb(0xd77757);
        Palette {
            background: rgb(0x0b0b0c),
            panel: rgb(0x111113),
            raised: rgb(0x18181b),
            border: rgb(0x26262a),
            foreground: rgb(0xe8e4dc),
            muted: rgb(0x8c8781),
            cursor: accent,
            selection: rgb(0x4a2c22),
            accent,
            success: rgb(0x4eba65),
            warning: rgb(0xffc107),
            error: rgb(0xff6b80),
            info: rgb(0xb1b9f9),
            added: rgb(0x0f2e18),
            removed: rgb(0x3a1219),
            added_emphasis: rgb(0x225c2b),
            removed_emphasis: rgb(0x7a2936),
            ansi: [
                rgb(0x1c1c1f),
                rgb(0xff6b80),
                rgb(0x4eba65),
                rgb(0xffc107),
                rgb(0xb1b9f9),
                rgb(0xd77757),
                rgb(0x48968c),
                rgb(0xd9d5cc),
                rgb(0x5a5753),
                rgb(0xff8c9c),
                rgb(0x6fd486),
                rgb(0xffd54f),
                rgb(0xcdd2ff),
                rgb(0xeb9a7e),
                rgb(0x6cb8ad),
                rgb(0xf5f2ea),
            ],
            syntax: "Monokai Extended".into(),
            mono_ui: true,
        }
    }

    /// The original blue-gray look.
    fn midnight() -> Palette {
        Palette {
            background: rgb(0x16181d),
            panel: rgb(0x1d2026),
            raised: rgb(0x23262d),
            border: rgb(0x2c3038),
            foreground: rgb(0xd4d7dd),
            muted: rgb(0x7d8490),
            cursor: rgb(0xe6e9ef),
            selection: rgb(0x344a6e),
            accent: rgb(0x7aa2f7),
            success: rgb(0x98c379),
            warning: rgb(0xe5c07b),
            error: rgb(0xe06c75),
            info: rgb(0x61afef),
            added: rgb(0x1f3a2a),
            removed: rgb(0x3f2228),
            added_emphasis: rgb(0x2f6a45),
            removed_emphasis: rgb(0x7a2f3a),
            ansi: [
                rgb(0x1d2026),
                rgb(0xe06c75),
                rgb(0x98c379),
                rgb(0xe5c07b),
                rgb(0x61afef),
                rgb(0xc678dd),
                rgb(0x56b6c2),
                rgb(0xabb2bf),
                rgb(0x5c6370),
                rgb(0xf07c85),
                rgb(0xa8d389),
                rgb(0xf5d08b),
                rgb(0x71bfff),
                rgb(0xd688ed),
                rgb(0x66c6d2),
                rgb(0xe6e9ef),
            ],
            syntax: "OneHalfDark".into(),
            mono_ui: false,
        }
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

    pub fn dim(&self, color: Color32) -> Color32 {
        lerp(self.background, color, 0.62)
    }
}

pub fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

pub fn parse_hex(text: &str) -> Option<Color32> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_palette() {
        assert_eq!(parse_hex("#7aa2f7"), Some(Color32::from_rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(parse_hex("nope"), None);
        let palette = Palette::from_config(&ThemeConfig::default());
        assert_eq!(palette.accent, rgb(0xd77757));
        let custom = ThemeConfig { accent: Some("#00ff00".into()), ..ThemeConfig::default() };
        assert_eq!(Palette::from_config(&custom).cursor, rgb(0x00ff00));
        let midnight = ThemeConfig { preset: "midnight".into(), ..ThemeConfig::default() };
        assert_eq!(Palette::from_config(&midnight).syntax, "OneHalfDark");
        assert_eq!(palette.indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(palette.indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(palette.indexed(232), Color32::from_rgb(8, 8, 8));
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
    }
}
