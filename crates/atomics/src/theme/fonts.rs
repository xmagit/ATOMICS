//! The desktop's theme fonts: an installed family found through the system's font list, or a font file.
//! A family that is not installed, or a file that cannot be read, leaves egui's font in place.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use atomics_ui::theme::{FontHook, FontSource, Fonts, definitions};
use egui::FontData;
use fontique::{Collection, CollectionOptions, FontStyle, FontWeight, FontWidth, SourceCache};

/// A theme's font value: paths are `~/`, absolute, or relative to the themes folder; anything else is a family name.
pub fn parse(value: &str, dir: &Path) -> FontSource {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let is_file = value.starts_with('~')
        || value.contains(['/', '\\'])
        || [".ttf", ".otf", ".ttc"].iter().any(|extension| lower.ends_with(extension));
    if !is_file {
        return FontSource::Family(value.into());
    }
    let path = atomics_core::paths::expand(value);
    FontSource::File(if path.is_absolute() { path } else { dir.join(path) })
}

/// Bumped by every font change, so a slow lookup never replaces a newer theme's fonts.
type Generation = std::sync::Arc<Mutex<u64>>;

/// The desktop's [`FontHook`]: families from the system's font list (fontique) and font files from disk.
pub struct SystemFonts;

impl FontHook for SystemFonts {
    /// Installs the theme's fonts when they changed. Finding a family reads the system's font
    /// list, which can take a moment, so that happens off the UI thread.
    fn apply(&self, ctx: &egui::Context, fonts: &Fonts) {
        let key = egui::Id::unique("atomics-theme-fonts");
        let wanted = (fonts.ui.clone(), fonts.mono.clone());
        if ctx.data(|d| d.get_temp::<(Option<FontSource>, Option<FontSource>)>(key)).as_ref() == Some(&wanted) {
            return;
        }
        ctx.data_mut(|d| d.insert_temp(key, wanted.clone()));
        let guard = ctx.data_mut(|d| d.get_temp_mut_or_default::<Generation>(key.with("generation")).clone());
        let generation = {
            let mut generation = guard.lock().unwrap();
            *generation += 1;
            *generation
        };
        if wanted == (None, None) {
            ctx.set_fonts(definitions(None, None, |_| None));
            return;
        }
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let definitions = definitions(wanted.0.as_ref(), wanted.1.as_ref(), load);
            let current = guard.lock().unwrap();
            if *current == generation {
                ctx.set_fonts(definitions);
                ctx.request_repaint();
            }
        });
    }
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
    use egui::FontFamily;

    #[cfg(target_os = "macos")]
    #[test]
    fn simple_mode_loads_both_bundled_geist_faces() {
        let ctx = egui::Context::default();
        SystemFonts.apply(&ctx, &Fonts::default());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            ctx.run_ui(egui::RawInput::default(), |_| {}).textures_delta.clear();
            let ready = ctx.fonts(|fonts| {
                let defs = fonts.definitions();
                [("simple", "geist"), ("simple-mono", "geist-mono")].into_iter().all(|(family, name)| {
                    defs.families
                        .get(&FontFamily::Name(family.into()))
                        .and_then(|faces| faces.first())
                        .is_some_and(|face| face == name)
                })
            });
            if ready {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "simple mode should load the interface and code faces");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn sources_tell_files_from_families() {
        let dir = Path::new("/config/themes");
        assert_eq!(parse(" Inter ", dir), FontSource::Family("Inter".into()));
        assert_eq!(parse("IBM Plex Mono", dir), FontSource::Family("IBM Plex Mono".into()));
        assert_eq!(parse("fonts/Inter.ttf", dir), FontSource::File(dir.join("fonts/Inter.ttf")));
        assert_eq!(parse("Inter.otf", dir), FontSource::File(dir.join("Inter.otf")));
        assert_eq!(parse("/fonts/a.ttc", dir), FontSource::File("/fonts/a.ttc".into()));
        assert_eq!(parse("~/fonts/a.ttf", dir), FontSource::File(atomics_core::paths::home().join("fonts/a.ttf")));
    }

    #[test]
    fn font_files_go_in_front_and_missing_fonts_keep_egui_defaults() {
        let dir = crate::test_support::tempdir().unwrap();
        let defaults = egui::FontDefinitions::default();
        let hack = defaults.font_data["Hack"].bytes().to_vec();
        let file = dir.path().join("mono.ttf");
        std::fs::write(&file, &hack).unwrap();
        let missing = FontSource::File(dir.path().join("missing.ttf"));
        let definitions = definitions(Some(&missing), Some(&FontSource::File(file)), load);
        assert_eq!(definitions.families[&FontFamily::Proportional], defaults.families[&FontFamily::Proportional]);
        let mono = &definitions.families[&FontFamily::Monospace];
        assert_eq!(mono[0], "theme-mono");
        assert_eq!(&mono[1..], &defaults.families[&FontFamily::Monospace][..], "egui's fonts stay as fallbacks");
        assert_eq!(definitions.font_data["theme-mono"].bytes(), &hack[..]);
    }
}
