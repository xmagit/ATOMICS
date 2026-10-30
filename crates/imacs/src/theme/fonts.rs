//! Theme fonts: an installed family found through the system's font list, or a font file.
//! A family that is not installed, or a file that cannot be read, leaves egui's font in place.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use egui::{FontData, FontDefinitions, FontFamily};
use fontique::{Collection, CollectionOptions, FontStyle, FontWeight, FontWidth, SourceCache};

#[derive(Clone, Debug, PartialEq)]
pub enum FontSource {
    Family(String),
    File(PathBuf),
}

impl FontSource {
    /// Paths are `~/`, absolute, or relative to the themes folder; anything else is a family name.
    pub fn parse(value: &str, dir: &Path) -> FontSource {
        let value = value.trim();
        let lower = value.to_ascii_lowercase();
        let is_file = value.starts_with('~')
            || value.contains(['/', '\\'])
            || [".ttf", ".otf", ".ttc"].iter().any(|extension| lower.ends_with(extension));
        if !is_file {
            return FontSource::Family(value.into());
        }
        let path = imacs_core::paths::expand(value);
        FontSource::File(if path.is_absolute() { path } else { dir.join(path) })
    }
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

/// Bumped by every font change, so a slow lookup never replaces a newer theme's fonts.
static GENERATION: Mutex<u64> = Mutex::new(0);

/// Installs the theme's fonts when they changed. Finding a family reads the system's font
/// list, which can take a moment, so that happens off the UI thread.
pub fn apply(ctx: &egui::Context, fonts: &Fonts) {
    let key = egui::Id::unique("imacs-theme-fonts");
    let wanted = (fonts.ui.clone(), fonts.mono.clone());
    if ctx.data(|d| d.get_temp::<(Option<FontSource>, Option<FontSource>)>(key)).unwrap_or_default() == wanted {
        return;
    }
    ctx.data_mut(|d| d.insert_temp(key, wanted.clone()));
    let generation = {
        let mut generation = GENERATION.lock().unwrap();
        *generation += 1;
        *generation
    };
    if wanted == (None, None) {
        ctx.set_fonts(FontDefinitions::default());
        return;
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let definitions = definitions(wanted.0.as_ref(), wanted.1.as_ref());
        let current = GENERATION.lock().unwrap();
        if *current == generation {
            ctx.set_fonts(definitions);
            ctx.request_repaint();
        }
    });
}

/// egui's fonts with the theme's in front; egui's stay behind them for missing glyphs.
pub fn definitions(ui: Option<&FontSource>, mono: Option<&FontSource>) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    for (source, family, name) in
        [(ui, FontFamily::Proportional, "theme-ui"), (mono, FontFamily::Monospace, "theme-mono")]
    {
        let Some(data) = source.and_then(load) else { continue };
        definitions.font_data.insert(name.into(), Arc::new(data));
        definitions.families.entry(family).or_default().insert(0, name.into());
    }
    definitions
}

fn load(source: &FontSource) -> Option<FontData> {
    match source {
        FontSource::File(path) => std::fs::read(path).ok().map(FontData::from_owned),
        FontSource::Family(name) => system_font(name),
    }
}

/// The regular face of an installed family.
fn system_font(name: &str) -> Option<FontData> {
    static SYSTEM: OnceLock<Mutex<(Collection, SourceCache)>> = OnceLock::new();
    let system = SYSTEM.get_or_init(|| {
        let collection = Collection::new(CollectionOptions { shared: false, system_fonts: true });
        Mutex::new((collection, SourceCache::default()))
    });
    let mut system = system.lock().unwrap();
    let (collection, cache) = &mut *system;
    let family = collection.family_by_name(name)?;
    let font = family.match_font(FontWidth::NORMAL, FontStyle::Normal, FontWeight::NORMAL, false)?;
    let blob = font.load(Some(cache))?;
    Some(FontData::from_blob(blob.into_raw_parts().0, font.index()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_tell_files_from_families() {
        let dir = Path::new("/config/themes");
        assert_eq!(FontSource::parse(" Inter ", dir), FontSource::Family("Inter".into()));
        assert_eq!(FontSource::parse("IBM Plex Mono", dir), FontSource::Family("IBM Plex Mono".into()));
        assert_eq!(FontSource::parse("fonts/Inter.ttf", dir), FontSource::File(dir.join("fonts/Inter.ttf")));
        assert_eq!(FontSource::parse("Inter.otf", dir), FontSource::File(dir.join("Inter.otf")));
        assert_eq!(FontSource::parse("/fonts/a.ttc", dir), FontSource::File("/fonts/a.ttc".into()));
        assert_eq!(
            FontSource::parse("~/fonts/a.ttf", dir),
            FontSource::File(imacs_core::paths::home().join("fonts/a.ttf"))
        );
    }

    #[test]
    fn font_files_go_in_front_and_missing_fonts_keep_egui_defaults() {
        let dir = crate::test_support::tempdir().unwrap();
        let defaults = FontDefinitions::default();
        let hack = defaults.font_data["Hack"].bytes().to_vec();
        let file = dir.path().join("mono.ttf");
        std::fs::write(&file, &hack).unwrap();
        let missing = FontSource::File(dir.path().join("missing.ttf"));
        let definitions = definitions(Some(&missing), Some(&FontSource::File(file)));
        assert_eq!(definitions.families[&FontFamily::Proportional], defaults.families[&FontFamily::Proportional]);
        let mono = &definitions.families[&FontFamily::Monospace];
        assert_eq!(mono[0], "theme-mono");
        assert_eq!(&mono[1..], &defaults.families[&FontFamily::Monospace][..], "egui's fonts stay as fallbacks");
        assert_eq!(definitions.font_data["theme-mono"].bytes(), &hack[..]);
    }
}
