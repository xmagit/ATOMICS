//! Themes: built-in presets plus `themes/<name>.toml` files beside the config.
//! A theme file sets any of the tokens below and inherits the rest with `extends`;
//! `[theme] name` picks a theme, and tokens set inline in `[theme]` win over it.

#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::ThemeConfig;

/// The theme used when none is named or the named one cannot be read.
pub const DEFAULT: &str = "claude";

/// Built-in themes, each a theme file that sets every token.
const BUILTIN: &[(&str, &str)] = &[
    ("claude", include_str!("../themes/claude.toml")),
    ("midnight", include_str!("../themes/midnight.toml")),
    ("paper", include_str!("../themes/paper.toml")),
    ("atomisk", include_str!("../themes/atomisk.toml")),
];

/// Theme files deeper than this are treated as a mistake rather than read forever.
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    /// The syntax theme a theme of this mode gets when it names none.
    pub fn default_syntax(self) -> &'static str {
        match self {
            ThemeMode::Dark => "OneHalfDark",
            ThemeMode::Light => "OneHalfLight",
        }
    }

    /// The built-in a theme file without `extends` inherits from.
    pub fn base(self) -> &'static str {
        match self {
            ThemeMode::Dark => DEFAULT,
            ThemeMode::Light => "paper",
        }
    }
}

/// Every value a theme can set. Colors are `#rrggbb`; anything left out comes from
/// the theme it extends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeTokens {
    /// Dark or light base widgets, and the default syntax theme.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ThemeMode>,
    /// The main area behind terminals and the diff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Bars and panels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    /// Headers, cards and buttons that sit above panels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raised: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topbar: Option<String>,
    /// Terminal background, also the answer to OSC 11 queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    /// Windows, menus and popups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<String>,
    /// Hints and the least important text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet: Option<String>,
    /// Outlines of buttons, fields and cards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    /// Lines between panels and sections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    /// Text and icons on accent fills.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_emphasis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_emphasis: Option<String>,
    /// Terminal colors 0–15: black, red, green, yellow, blue, magenta, cyan, white, then
    /// their bright versions. A shorter list replaces only the first colors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ansi: Option<Vec<String>>,
    /// A bat/two-face theme for the diff and file view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syntax: Option<String>,
    /// Monospace for all UI text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_ui: Option<bool>,
    /// An installed font family or a font file, for UI text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_font: Option<String>,
    /// An installed font family or a font file, for terminals, the diff and code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_font: Option<String>,
    /// Base size of UI text; the whole UI scales with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_size: Option<f32>,
    /// Terminal text size, over `[font] size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_size: Option<f32>,
    /// Diff and editor text size, over `[font] diff_size`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_size: Option<f32>,
}

macro_rules! color_tokens {
    ($($name:ident),* $(,)?) => {
        impl ThemeTokens {
            /// Names of the color tokens.
            pub const COLORS: &'static [&'static str] = &[$(stringify!($name)),*];

            /// Every color token with its name.
            pub fn colors(&self) -> Vec<(&'static str, Option<&str>)> {
                vec![$((stringify!($name), self.$name.as_deref())),*]
            }

            fn colors_mut(&mut self) -> Vec<(&'static str, &mut Option<String>)> {
                vec![$((stringify!($name), &mut self.$name)),*]
            }
        }
    };
}

color_tokens!(
    background,
    panel,
    raised,
    sidebar,
    topbar,
    terminal,
    overlay,
    foreground,
    muted,
    quiet,
    border,
    separator,
    accent,
    on_accent,
    selection,
    cursor,
    success,
    warning,
    error,
    info,
    added,
    removed,
    added_emphasis,
    removed_emphasis,
);

/// Every key a theme file may set besides `extends`.
pub fn token_keys() -> impl Iterator<Item = &'static str> {
    ["mode"].into_iter().chain(ThemeTokens::COLORS.iter().copied()).chain([
        "ansi",
        "syntax",
        "mono_ui",
        "ui_font",
        "mono_font",
        "ui_size",
        "mono_size",
        "diff_size",
    ])
}

impl ThemeTokens {
    /// Takes every token `layer` sets. Switching mode without naming a syntax theme
    /// picks that mode's default, so a light theme never inherits dark code colors.
    pub fn apply(&mut self, layer: &ThemeTokens) {
        if let Some(mode) = layer.mode
            && self.mode != Some(mode)
            && layer.syntax.is_none()
        {
            self.syntax = Some(mode.default_syntax().into());
        }
        for ((_, target), (_, value)) in self.colors_mut().into_iter().zip(layer.colors()) {
            if let Some(value) = value {
                *target = Some(value.to_string());
            }
        }
        if let Some(colors) = &layer.ansi {
            let ansi = self.ansi.get_or_insert_with(Vec::new);
            for (index, color) in colors.iter().enumerate() {
                match ansi.get_mut(index) {
                    Some(slot) => *slot = color.clone(),
                    None => ansi.push(color.clone()),
                }
            }
        }
        macro_rules! take {
            ($($field:ident),*) => {$(
                if layer.$field.is_some() {
                    self.$field = layer.$field.clone();
                }
            )*};
        }
        take!(mode, syntax, mono_ui, ui_font, mono_font, ui_size, mono_size, diff_size);
    }

    /// Drops values that cannot be used and says why, so one typo keeps the rest of a theme.
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        for (name, value) in self.colors_mut() {
            if let Some(text) = value.as_deref()
                && parse_color(text).is_none()
            {
                problems.push(format!("{name} = \"{text}\" is not a #rrggbb color"));
                *value = None;
            }
        }
        if let Some(ansi) = &self.ansi {
            if ansi.len() > 16 {
                problems.push(format!("ansi has {} colors; the terminal has 16", ansi.len()));
                self.ansi = None;
            } else if let Some(bad) = ansi.iter().find(|color| parse_color(color).is_none()) {
                problems.push(format!("ansi color \"{bad}\" is not a #rrggbb color"));
                self.ansi = None;
            }
        }
        for (name, size) in
            [("ui_size", &mut self.ui_size), ("mono_size", &mut self.mono_size), ("diff_size", &mut self.diff_size)]
        {
            if let Some(value) = *size
                && !(6.0..=48.0).contains(&value)
            {
                problems.push(format!("{name} = {value} is outside 6–48"));
                *size = None;
            }
        }
        for font in [&mut self.ui_font, &mut self.mono_font] {
            if font.as_deref().is_some_and(|name| name.trim().is_empty()) {
                *font = None;
            }
        }
        problems
    }
}

/// A theme file: tokens, and the theme they extend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeFile {
    /// A built-in theme or another theme file. Without it a file extends the
    /// built-in base of its mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(flatten)]
    pub tokens: ThemeTokens,
}

impl ThemeFile {
    /// Parses a theme file. Unknown keys and unusable values are listed and left out;
    /// only text that is not a theme at all is an error.
    pub fn parse(text: &str) -> Result<(ThemeFile, Vec<String>)> {
        let table: toml::Table = toml::from_str(text)?;
        let mut problems: Vec<String> = table
            .keys()
            .filter(|key| *key != "extends" && !token_keys().any(|known| known == *key))
            .map(|key| format!("unknown key {key}"))
            .collect();
        let mut file: ThemeFile = toml::Value::Table(table).try_into()?;
        problems.extend(file.tokens.sanitize());
        Ok((file, problems))
    }
}

/// Parses `#rrggbb` (the `#` is optional).
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim();
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

/// Theme names are file names: letters, digits, `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    BUILTIN.iter().map(|(name, _)| *name)
}

/// A built-in theme's tokens; built-in names ignore case.
pub fn builtin(name: &str) -> Option<ThemeTokens> {
    let (_, text) = BUILTIN.iter().find(|(builtin, _)| builtin.eq_ignore_ascii_case(name))?;
    let (file, _) = ThemeFile::parse(text).expect("built-in themes parse");
    Some(file.tokens)
}

/// A theme resolved down to a built-in, with every token set.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub tokens: ThemeTokens,
    /// The themes applied, from the selected one down to its built-in.
    pub chain: Vec<String>,
    /// Problems with theme files or `[theme]`; everything that could be read still applies.
    pub errors: Vec<String>,
}

/// Resolves `[theme]`: the named theme, the themes it extends, then the inline tokens.
/// `read` returns a theme file's text, or `None` when there is no file of that name.
/// A theme file shadows a built-in of the same name and may extend it.
pub fn resolve(config: &ThemeConfig, read: impl Fn(&str) -> Option<Result<String, String>>) -> Resolved {
    let mut errors = Vec::new();
    let mut layers = Vec::new();
    let mut chain: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let selected = config.name.trim();
    let mut next = Some(if selected.is_empty() { DEFAULT.to_string() } else { selected.to_string() });
    while let Some(name) = next.take() {
        if chain.len() >= MAX_DEPTH {
            errors.push(format!("theme {} extends more than {MAX_DEPTH} themes", chain[0]));
            break;
        }
        let text = if files.contains(&name) || !valid_name(&name) { None } else { read(&name) };
        let Some(text) = text else {
            match builtin(&name) {
                Some(tokens) => {
                    layers.push(tokens);
                    chain.push(name);
                }
                None if files.contains(&name) => {
                    errors.push(format!("themes {} extend each other in a cycle", chain.join(" → ")))
                }
                None => errors.push(format!("unknown theme \"{name}\"")),
            }
            break;
        };
        files.push(name.clone());
        match text.map_err(anyhow::Error::msg).and_then(|text| ThemeFile::parse(&text)) {
            Ok((file, problems)) => {
                errors.extend(problems.into_iter().map(|problem| format!("themes/{name}.toml: {problem}")));
                next = Some(
                    file.extends.clone().unwrap_or_else(|| file.tokens.mode.unwrap_or(ThemeMode::Dark).base().into()),
                );
                layers.push(file.tokens);
                chain.push(name);
            }
            Err(err) => errors.push(format!("themes/{name}.toml: {err:#}")),
        }
    }
    // A broken chain still gets every token, from the default theme.
    let mut tokens = builtin(DEFAULT).expect("default theme is built in");
    for layer in layers.iter().rev() {
        tokens.apply(layer);
    }
    let mut inline = config.tokens.clone();
    errors.extend(inline.sanitize().into_iter().map(|problem| format!("[theme]: {problem}")));
    tokens.apply(&inline);
    Resolved { tokens, chain, errors }
}

/// Theme files live in `themes/` next to the config file.
#[cfg(feature = "native")]
pub fn themes_dir(config_file: &Path) -> PathBuf {
    config_file.parent().unwrap_or_else(|| Path::new(".")).join("themes")
}

/// Checks a proposed theme file strictly, as an agent's change must be: a usable name,
/// every key known, every value usable, and the theme resolving without problems.
/// `read` reads the other theme files. Returns what the user should know before applying.
pub fn check(
    name: &str,
    text: &str,
    read: impl Fn(&str) -> Option<Result<String, String>>,
) -> Result<Vec<String>, String> {
    if !valid_name(name) {
        return Err(format!("theme names use letters, digits, - and _, not \"{name}\""));
    }
    let (_, problems) = ThemeFile::parse(text).map_err(|err| format!("themes/{name}.toml: {err:#}"))?;
    if !problems.is_empty() {
        return Err(format!("themes/{name}.toml: {}", problems.join("; ")));
    }
    let config = ThemeConfig { name: name.into(), ..ThemeConfig::default() };
    let resolved = resolve(&config, |file| if file == name { Some(Ok(text.to_string())) } else { read(file) });
    if !resolved.errors.is_empty() {
        return Err(resolved.errors.join("; "));
    }
    let mut warnings = Vec::new();
    if builtin(name).is_some() {
        warnings.push(format!("themes/{name}.toml replaces the built-in {name} theme"));
    }
    Ok(warnings)
}

/// A theme file's text, `None` when there is no file of that name.
#[cfg(feature = "native")]
pub fn read_file(dir: &Path, name: &str) -> Option<Result<String, String>> {
    match std::fs::read_to_string(dir.join(format!("{name}.toml"))) {
        Ok(text) => Some(Ok(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => Some(Err(err.to_string())),
    }
}

/// Resolves `[theme]` against the theme files in `dir`.
#[cfg(feature = "native")]
pub fn load(config: &ThemeConfig, dir: &Path) -> Resolved {
    resolve(config, |name| read_file(dir, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn theme(name: &str) -> ThemeConfig {
        ThemeConfig { name: name.into(), ..ThemeConfig::default() }
    }

    fn files(entries: &[(&str, &str)]) -> impl Fn(&str) -> Option<Result<String, String>> {
        let map: HashMap<String, String> = entries.iter().map(|(n, t)| (n.to_string(), t.to_string())).collect();
        move |name| map.get(name).cloned().map(Ok)
    }

    /// Every token but fonts and sizes, which default to egui's fonts and `[font]`.
    fn complete(tokens: &ThemeTokens) -> bool {
        tokens.mode.is_some()
            && tokens.colors().iter().all(|(_, value)| value.is_some())
            && tokens.ansi.as_ref().is_some_and(|ansi| ansi.len() == 16)
            && tokens.syntax.is_some()
            && tokens.mono_ui.is_some()
    }

    #[test]
    fn token_keys_name_every_field() {
        let mut tokens = builtin(DEFAULT).unwrap();
        tokens.ui_font = Some("Inter".into());
        tokens.mono_font = Some("IBM Plex Mono".into());
        (tokens.ui_size, tokens.mono_size, tokens.diff_size) = (Some(13.0), Some(13.0), Some(12.5));
        let table: toml::Table = toml::from_str(&toml::to_string(&tokens).unwrap()).unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut known: Vec<&str> = token_keys().collect();
        keys.sort();
        known.sort();
        assert_eq!(keys, known);
    }

    #[test]
    fn built_in_themes_set_every_token() {
        for name in builtin_names() {
            let (file, problems) = ThemeFile::parse(BUILTIN.iter().find(|(n, _)| *n == name).unwrap().1).unwrap();
            assert!(problems.is_empty(), "{name}: {problems:?}");
            assert!(file.extends.is_none(), "{name} must not extend");
            assert!(complete(&file.tokens), "{name} must set every token");
            assert_eq!(file.tokens.ansi.as_ref().unwrap().len(), 16, "{name}");
        }
        assert!(builtin("Midnight").is_some(), "built-in names ignore case");
    }

    #[test]
    fn files_extend_themes_and_inline_tokens_win() {
        let read = files(&[
            (
                "base",
                "extends = \"midnight\"\naccent = \"#112233\"\npanel = \"#000001\"\nansi = [\"#010101\", \"#020202\"]",
            ),
            ("mine", "extends = \"base\"\naccent = \"#445566\"\nui_font = \"Inter\"\nmono_size = 14"),
        ]);
        let mut config = theme("mine");
        config.tokens.panel = Some("#abcdef".into());
        let resolved = resolve(&config, read);
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        assert_eq!(resolved.chain, ["mine", "base", "midnight"]);
        let tokens = &resolved.tokens;
        assert!(complete(tokens));
        assert_eq!(tokens.accent.as_deref(), Some("#445566"), "the selected file wins over what it extends");
        assert_eq!(tokens.panel.as_deref(), Some("#abcdef"), "inline [theme] tokens win over files");
        assert_eq!(tokens.syntax, builtin("midnight").unwrap().syntax, "untouched tokens come from the built-in");
        assert_eq!(tokens.ui_font.as_deref(), Some("Inter"));
        assert_eq!(tokens.mono_size, Some(14.0));
        let ansi = tokens.ansi.as_ref().unwrap();
        assert_eq!(ansi.len(), 16);
        assert_eq!(&ansi[..2], ["#010101", "#020202"], "a short ansi list replaces the first colors");
        assert_eq!(ansi[2], builtin("midnight").unwrap().ansi.unwrap()[2]);
    }

    #[test]
    fn selection_by_name_files_shadow_built_ins_and_errors_fall_back() {
        let read = files(&[
            ("loop-a", "extends = \"loop-b\""),
            ("loop-b", "extends = \"loop-a\""),
            ("typo", "accent = \"orange\"\nbackgroud = \"#000000\"\nwarning = \"#aabbcc\""),
            ("broken", "accent = "),
        ]);
        let shadowed = resolve(&theme("claude"), files(&[("claude", "extends = \"claude\"\naccent = \"#123456\"")]));
        assert_eq!(shadowed.chain, ["claude", "claude"], "a file may extend the built-in it shadows");
        assert_eq!(shadowed.tokens.accent.as_deref(), Some("#123456"));
        let midnight = resolve(&theme("midnight"), &read);
        assert_eq!(midnight.tokens, builtin("midnight").unwrap(), "[theme] name selects a built-in");
        let unknown = resolve(&theme("nope"), &read);
        assert_eq!(unknown.tokens, builtin(DEFAULT).unwrap());
        assert_eq!(unknown.errors, ["unknown theme \"nope\""]);
        let cycle = resolve(&theme("loop-a"), &read);
        assert!(cycle.errors[0].contains("cycle"), "{:?}", cycle.errors);
        let typo = resolve(&theme("typo"), &read);
        assert_eq!(typo.errors.len(), 2, "{:?}", typo.errors);
        assert_eq!(typo.tokens.warning.as_deref(), Some("#aabbcc"), "valid tokens survive a typo");
        assert_eq!(typo.tokens.accent, builtin(DEFAULT).unwrap().accent);
        let broken = resolve(&theme("broken"), &read);
        assert_eq!(broken.tokens, builtin(DEFAULT).unwrap());
        assert!(broken.errors[0].starts_with("themes/broken.toml:"), "{:?}", broken.errors);
        let escape = resolve(&theme("../config"), |_| panic!("names with paths are never read"));
        assert_eq!(escape.errors, ["unknown theme \"../config\""]);
        let legacy: crate::config::Config =
            toml::from_str("[theme]\npreset = \"midnight\"\naccent = \"#00ff00\"").unwrap();
        let resolved = resolve(&legacy.theme, |_| None);
        assert_eq!(resolved.chain, ["midnight"], "preset still selects a theme");
        assert_eq!(resolved.tokens.accent.as_deref(), Some("#00ff00"));
    }

    #[test]
    fn atomisk_follows_the_atomisk_stylesheet() {
        let atomisk = builtin("atomisk").unwrap();
        let get = |name: &str| atomisk.colors().into_iter().find(|(n, _)| *n == name).unwrap().1.unwrap().to_string();
        for (token, value) in [
            ("background", "#08090a"),
            ("panel", "#101113"),
            ("foreground", "#f1f2f3"),
            ("muted", "#95989e"),
            ("quiet", "#72777e"),
            ("accent", "#c5decf"),
            ("on_accent", "#08090a"),
            ("info", "#adc8dc"),
            ("warning", "#e4d1a8"),
        ] {
            assert_eq!(get(token), value, "{token}");
        }
        // --line is rgba(230,237,244,.14) over void.
        let over_void = |c: u8, v: u8| (0.14 * c as f32 + 0.86 * v as f32).round() as u8;
        let [r, g, b] = parse_color(&get("border")).unwrap();
        assert_eq!([r, g, b], [over_void(230, 0x08), over_void(237, 0x09), over_void(244, 0x0a)]);
        let lighter = |token: &str| parse_color(&get(token)).unwrap().iter().map(|&c| c as u32).sum::<u32>();
        assert!(lighter("raised") > lighter("panel"), "raised sits slightly above surface");
        assert_eq!(atomisk.ui_font.as_deref(), Some("Inter"));
        assert_eq!(atomisk.mono_font.as_deref(), Some("IBM Plex Mono"));
    }

    #[test]
    fn light_themes_get_light_bases_and_syntax() {
        let read = files(&[
            ("claude-light", "extends = \"claude\"\nmode = \"light\"\nbackground = \"#ffffff\""),
            ("named", "extends = \"claude\"\nmode = \"light\"\nsyntax = \"GitHub\""),
            ("sepia", "mode = \"light\"\nbackground = \"#f4ecd8\""),
        ]);
        let switched = resolve(&theme("claude-light"), &read).tokens;
        assert_eq!(switched.syntax.as_deref(), Some("OneHalfLight"), "a mode switch drops the dark syntax theme");
        assert_eq!(switched.accent, builtin("claude").unwrap().accent, "everything else is still inherited");
        assert_eq!(resolve(&theme("named"), &read).tokens.syntax.as_deref(), Some("GitHub"));
        let sepia = resolve(&theme("sepia"), &read);
        assert_eq!(sepia.chain, ["sepia", "paper"], "a light file without extends builds on the light base");
        assert_eq!(sepia.tokens.foreground, builtin("paper").unwrap().foreground);
        let paper = builtin("paper").unwrap();
        assert_eq!(paper.mode, Some(ThemeMode::Light));
        assert_eq!(paper.syntax.as_deref(), Some(ThemeMode::Light.default_syntax()));
        let mut inline = theme("claude");
        inline.tokens.mode = Some(ThemeMode::Light);
        assert_eq!(resolve(&inline, |_| None).tokens.syntax.as_deref(), Some("OneHalfLight"));
    }

    #[test]
    fn invalid_values_are_reported_and_dropped() {
        let (file, problems) = ThemeFile::parse(
            "ansi = [\"#000000\", \"red\"]\nui_size = 200\nmono_font = \"  \"\nselection = \"#12345\"\ncursor = \"abcdef\"",
        )
        .unwrap();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(file.tokens.ansi.is_none() && file.tokens.ui_size.is_none() && file.tokens.selection.is_none());
        assert!(file.tokens.mono_font.is_none());
        assert_eq!(file.tokens.cursor.as_deref(), Some("abcdef"), "the # is optional");
        assert!(ThemeFile::parse("mode = \"sepia\"").is_err(), "a value of the wrong kind is not a theme");
        assert_eq!(parse_color("#7aa2f7"), Some([0x7a, 0xa2, 0xf7]));
        assert_eq!(parse_color("#+1+2+3"), None);
    }

    #[test]
    fn proposed_theme_files_are_checked_strictly() {
        let read = files(&[("base", "extends = \"midnight\""), ("loop", "extends = \"dusk\"")]);
        assert_eq!(check("dusk", "extends = \"base\"\naccent = \"#e0a458\"", &read), Ok(vec![]));
        assert_eq!(
            check("claude", "extends = \"claude\"\naccent = \"#e0a458\"", &read).unwrap(),
            ["themes/claude.toml replaces the built-in claude theme"]
        );
        for (name, text, problem) in [
            ("dusk", "acent = \"#e0a458\"", "unknown key acent"),
            ("dusk", "accent = \"orange\"", "accent = \"orange\" is not a #rrggbb color"),
            ("dusk", "extends = \"nope\"", "unknown theme \"nope\""),
            ("dusk", "extends = \"loop\"", "cycle"),
            ("dusk", "mode = \"sepia\"", "themes/dusk.toml:"),
            ("../dusk", "", "theme names use"),
        ] {
            let error = check(name, text, &read).unwrap_err();
            assert!(error.contains(problem), "{text}: {error}");
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn theme_files_load_from_the_themes_folder() {
        let dir = tempfile::tempdir().unwrap();
        let config_file = dir.path().join("config.toml");
        let themes = themes_dir(&config_file);
        assert_eq!(themes, dir.path().join("themes"));
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("mine.toml"), "extends = \"midnight\"\nforeground = \"#fafafa\"").unwrap();
        let resolved = load(&theme("mine"), &themes);
        assert_eq!(resolved.chain, ["mine", "midnight"]);
        assert_eq!(resolved.tokens.foreground.as_deref(), Some("#fafafa"));
        assert_eq!(load(&theme("missing"), &themes).errors, ["unknown theme \"missing\""]);
    }
}
