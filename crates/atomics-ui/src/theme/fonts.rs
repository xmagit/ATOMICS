//! Theme fonts: what a theme names, and how named faces join egui's fonts. Finding a face is the host's:
//! the desktop looks families up in the system's font list and reads font files; a browser serves fonts
//! it bundles. A face that cannot be found leaves egui's font in place.

use std::path::PathBuf;
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

/// A font a theme names: an installed family, or a font file on the host.
#[derive(Clone, Debug, PartialEq)]
pub enum FontSource {
    Family(String),
    File(PathBuf),
}

/// A theme's fonts and sizes; sizes left out come from `[font]`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fonts {
    pub ui: Option<FontSource>,
    pub mono: Option<FontSource>,
    pub ui_size: Option<f32>,
    pub mono_size: Option<f32>,
    pub diff_size: Option<f32>,
}

/// Installs a theme's fonts in egui; [`crate::theme::Palette::apply`] calls it. The host supplies it,
/// since only the host can find a face: it builds the fonts with [`definitions`].
pub trait FontHook {
    fn apply(&self, ctx: &egui::Context, fonts: &Fonts);
}

/// egui's fonts with the theme's in front; egui's stay behind them for missing glyphs. `load` finds a face.
pub fn definitions(
    ui: Option<&FontSource>,
    mono: Option<&FontSource>,
    load: impl Fn(&FontSource) -> Option<FontData>,
) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    for (source, family, name) in
        [(ui, FontFamily::Proportional, "theme-ui"), (mono, FontFamily::Monospace, "theme-mono")]
    {
        let Some(data) = source.and_then(&load) else { continue };
        definitions.font_data.insert(name.into(), Arc::new(data));
        definitions.families.entry(family).or_default().insert(0, name.into());
    }
    // The simple view's face: the proportional one, which a platform may put a face of its own in front of.
    let proportional = definitions.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    definitions.families.insert(FontFamily::Name(SIMPLE_FAMILY.into()), proportional);
    definitions
}

/// The font family the simple view draws its text in ([`crate::flow::font`]).
pub const SIMPLE_FAMILY: &str = "simple";

thread_local! {
    static SIMPLE_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this frame's context has the simple view's family; the view's text falls back to the proportional
/// one where it does not (a context whose fonts did not come from [`definitions`]). Called as the view starts.
pub fn simple_family(ctx: &egui::Context) -> bool {
    let ready = ctx.fonts(|fonts| fonts.families().contains(&FontFamily::Name(SIMPLE_FAMILY.into())));
    SIMPLE_READY.with(|cell| cell.set(ready));
    ready
}

/// The simple view's family, where this frame has it.
pub fn simple_font_family() -> FontFamily {
    if SIMPLE_READY.with(|cell| cell.get()) { FontFamily::Name(SIMPLE_FAMILY.into()) } else { FontFamily::Proportional }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_faces_go_in_front_and_missing_ones_keep_egui_defaults() {
        let defaults = FontDefinitions::default();
        let hack = defaults.font_data["Hack"].bytes().to_vec();
        let found = FontSource::Family("Found".into());
        let load = |source: &FontSource| (*source == found).then(|| FontData::from_owned(hack.clone()));
        let definitions = definitions(Some(&FontSource::Family("Missing".into())), Some(&found), load);
        assert_eq!(definitions.families[&FontFamily::Proportional], defaults.families[&FontFamily::Proportional]);
        let mono = &definitions.families[&FontFamily::Monospace];
        assert_eq!(mono[0], "theme-mono");
        assert_eq!(&mono[1..], &defaults.families[&FontFamily::Monospace][..], "egui's fonts stay as fallbacks");
        assert_eq!(definitions.font_data["theme-mono"].bytes(), &hack[..]);
    }
}
