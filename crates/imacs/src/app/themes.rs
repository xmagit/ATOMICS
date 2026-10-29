//! The theme picker in the Settings menu: every built-in theme and theme file, each with a strip
//! of its own colors. A click shows the theme at once and saves `[theme] name` the way an applied
//! config change is saved: comments kept, a backup, an atomic write, then a reload.
use super::*;
use imacs_core::config::ThemeConfig;
use imacs_core::selfconfig;

/// Width of a picker row; the menu grows to fit it.
const ROW_WIDTH: f32 = 300.0;

/// A theme the picker offers.
pub(super) struct ThemeChoice {
    pub(super) name: String,
    /// `themes/<name>.toml`, which may replace the built-in theme of that name.
    pub(super) file: bool,
    /// Background, panel, foreground, accent, then red, green and blue from its ANSI colors.
    pub(super) swatches: [Color32; 7],
}

/// The built-in themes, then the theme files in `dir`. A file named like a built-in theme
/// takes its place, since that is the theme the name now shows.
pub(super) fn theme_choices(dir: &Path) -> Vec<ThemeChoice> {
    let files = imacs_core::theme::file_names(dir);
    let builtin: Vec<&str> = imacs_core::theme::builtin_names().collect();
    let names = builtin
        .iter()
        .map(|name| (name.to_string(), files.iter().any(|file| file == name)))
        .chain(files.iter().filter(|file| !builtin.contains(&file.as_str())).map(|file| (file.clone(), true)));
    names
        .map(|(name, file)| {
            let (p, _) = Palette::load(&ThemeConfig { name: name.clone(), ..ThemeConfig::default() }, dir);
            let swatches = [p.background, p.panel, p.foreground, p.accent, p.ansi[1], p.ansi[2], p.ansi[4]];
            ThemeChoice { name, file, swatches }
        })
        .collect()
}

/// Sets `[theme] name` in `file` as an applied config change is written: comments kept, a
/// backup, an atomic replace. Returns whether the file changed.
fn write_theme_name(file: &Path, name: &str, stamp: u64) -> Result<bool, String> {
    let shown = paths::display(file);
    let before = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(format!("reading {shown}: {err}")),
    };
    let after =
        selfconfig::with_theme_name(before.as_deref().unwrap_or(""), name).map_err(|err| format!("{shown}: {err}"))?;
    let Some(after) = after else { return Ok(false) };
    selfconfig::write(file, before.as_deref(), &after, stamp).map_err(|err| format!("{err:#}"))?;
    Ok(true)
}

impl State {
    /// The picker's themes, and whether this machine's file picks one; read when the config
    /// or a theme file changes, never per frame.
    pub(super) fn refresh_themes(&mut self) {
        self.theme_choices = Arc::new(theme_choices(&imacs_core::theme::themes_dir(&self.config_path)));
        let host = std::fs::read_to_string(&Config::source_paths(&self.config_path)[1]).unwrap_or_default();
        self.host_theme = selfconfig::picks_theme(&host);
    }

    /// The user picked a theme: save its name to `config.toml`, or to this machine's file, and
    /// show it at once. The user's own click, so no proposal.
    pub(super) fn select_theme(&mut self, name: &str, host: bool) {
        let [main, host_file] = Config::source_paths(&self.config_path);
        let file = if host { host_file } else { main };
        match write_theme_name(&file, name, imacs_core::store::now()) {
            Ok(changed) => {
                if changed {
                    self.reload_config();
                }
                let shown = paths::display(&file);
                self.toast(if !host && self.host_theme {
                    format!("Theme {name} saved to {shown}, but this machine's config.d file picks its own")
                } else {
                    format!("Theme {name} saved to {shown}")
                });
            }
            Err(err) => self.toast(format!("Theme not saved: {err}")),
        }
    }
}

impl Desktop<'_> {
    /// The Themes section of the Settings menu.
    pub(super) fn themes_menu(&mut self, ui: &mut egui::Ui) {
        let p = self.runtime.palette.clone();
        ui.label(RichText::new("Themes").strong());
        let current = self.runtime.config.theme.name.trim();
        let mut picked = None;
        for choice in self.runtime.theme_choices.iter() {
            let selected = choice.name == current || (!choice.file && choice.name.eq_ignore_ascii_case(current));
            let (rect, response) = ui.allocate_exact_size(vec2(ROW_WIDTH, 22.0), Sense::click());
            let painter = ui.painter();
            if selected {
                painter.rect_filled(rect, 2.0, p.selection);
            } else if response.hovered() {
                painter.rect_filled(rect, 2.0, p.hover());
            }
            let y = rect.center().y;
            if selected {
                painter.text(pos2(rect.left() + 5.0, y), egui::Align2::LEFT_CENTER, "✓", p.ui(12.0), p.accent);
            }
            let strip = Rect::from_min_size(pos2(rect.left() + 22.0, y - 6.0), vec2(12.0 * 7.0, 12.0));
            for (i, color) in choice.swatches.iter().enumerate() {
                let cell = Rect::from_min_size(strip.min + vec2(12.0 * i as f32, 0.0), vec2(12.0, 12.0));
                painter.rect_filled(cell, 0.0, *color);
            }
            painter.rect_stroke(strip, 1.0, Stroke::new(1.0, p.border), egui::StrokeKind::Outside);
            let name_color = if selected { p.accent } else { p.foreground };
            painter.text(
                pos2(strip.right() + 10.0, y),
                egui::Align2::LEFT_CENTER,
                &choice.name,
                p.ui(12.5),
                name_color,
            );
            if choice.file {
                painter.text(
                    rect.right_center() - vec2(6.0, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    "yours",
                    p.ui(11.0),
                    p.quiet,
                );
            }
            let hover = if choice.file { format!("themes/{}.toml", choice.name) } else { "Built in".to_string() };
            if response.on_hover_text(hover).clicked() && !selected {
                picked = Some(choice.name.clone());
            }
        }
        let host_file = Config::source_paths(&self.runtime.config_path)[1].clone();
        let host_name = host_file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        ui.checkbox(&mut self.client.theme_host_only, format!("This machine only (config.d/{host_name})"));
        let host = self.client.theme_host_only;
        if !host && self.runtime.host_theme {
            ui.label(
                RichText::new(format!("config.d/{host_name} picks this machine's theme and wins over config.toml"))
                    .color(p.warning)
                    .size(11.5),
            );
        }
        let inline = self.runtime.config.theme.tokens.set_keys();
        if !inline.is_empty() {
            ui.label(
                RichText::new(format!("Set in [theme], so shown over any theme: {}", inline.join(", ")))
                    .color(p.muted)
                    .size(11.5),
            );
        }
        if let Some(error) = &self.runtime.theme_error {
            ui.label(RichText::new(format!("theme: {error}")).color(p.warning).size(11.5));
        }
        if let Some(name) = picked {
            self.post(move |state| state.select_theme(&name, host));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, with_app};
    use super::*;

    fn render(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
        let time = ctx.input(|i| i.time) + 0.1;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| {
                egui::Panel::top("top").exact_size(34.0).show(ui, |ui| app.top_bar(ui));
            },
        );
        app.runtime.drain_actions();
        output
    }

    /// Where `label` was drawn on a settled frame.
    fn find(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str) -> Option<Pos2> {
        render(app, ctx, vec![]).drop_without_applying_deltas();
        let output = render(app, ctx, vec![]);
        let found = output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == label => {
                Some(text.pos + text.galley.rect.center().to_vec2())
            }
            _ => None,
        });
        output.drop_without_applying_deltas();
        found
    }

    fn click(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str) {
        let pos = find(app, ctx, label).unwrap_or_else(|| panic!("{label} is not on screen"));
        for pressed in [true, false] {
            let button = egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            };
            render(app, ctx, vec![egui::Event::PointerMoved(pos), button]).drop_without_applying_deltas();
        }
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "bak"))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn the_settings_menu_picks_themes_and_saves_their_names() {
        with_app("app::themes::tests::the_settings_menu_picks_themes_and_saves_their_names", |app, ctx| {
            let main = app.runtime.config_path.clone();
            let [_, host] = Config::source_paths(&main);
            let folder = main.parent().unwrap().to_path_buf();
            let themes = imacs_core::theme::themes_dir(&main);
            std::fs::create_dir_all(&themes).unwrap();
            std::fs::write(themes.join("dusk.toml"), "extends = \"midnight\"\naccent = \"#e0a458\"\n").unwrap();
            std::fs::write(&main, imacs_core::config::DEFAULT_CONFIG).unwrap();
            app.runtime.reload_config();
            let listed: Vec<_> = app.runtime.theme_choices.iter().map(|c| (c.name.as_str(), c.file)).collect();
            assert_eq!(
                listed,
                [
                    ("claude", false),
                    ("midnight", false),
                    ("paper", false),
                    ("atomisk", false),
                    ("tide", false),
                    ("dusk", true)
                ]
            );
            let dusk = &app.runtime.theme_choices[5];
            assert_eq!(dusk.swatches[3], crate::theme::parse_hex("#e0a458").unwrap(), "the strip shows its accent");
            let built_in = request(app, json!({"cmd":"config_get"})).result["themes"]["built_in"].clone();
            assert_eq!(built_in, json!(["claude", "midnight", "paper", "atomisk", "tide"]));

            // The menu stays open while the user tries themes and toggles where they are saved.
            click(app, ctx, "Settings");
            for name in ["claude", "atomisk", "tide", "dusk", "yours"] {
                assert!(find(app, ctx, name).is_some(), "{name} is listed");
            }
            click(app, ctx, "atomisk");
            let atomisk = Palette::from_config(&ThemeConfig { name: "atomisk".into(), ..ThemeConfig::default() });
            assert_eq!(app.runtime.palette.accent, atomisk.accent, "a picked theme shows at once");
            let written = std::fs::read_to_string(&main).unwrap();
            assert!(
                written.contains("name = \"atomisk\"             # built in: claude, midnight, atomisk"),
                "the value is replaced in place and keeps its comment"
            );
            assert!(written.contains("# BEGIN RELAY PROFILE"), "the rest of the file stays as it was");
            let backup = backups(&folder);
            assert_eq!(backup.len(), 1);
            assert_eq!(std::fs::read_to_string(&backup[0]).unwrap(), imacs_core::config::DEFAULT_CONFIG);
            assert!(!host.exists());

            let host_name = host.file_name().unwrap().to_string_lossy().into_owned();
            click(app, ctx, &format!("This machine only (config.d/{host_name})"));
            assert!(find(app, ctx, "tide").is_some(), "the toggle leaves the menu open");
            assert!(app.client.theme_host_only);
            click(app, ctx, "tide");
            assert_eq!(std::fs::read_to_string(&host).unwrap(), "[theme]\nname = \"tide\"\n");
            assert!(
                std::fs::read_to_string(&main).unwrap().contains("name = \"atomisk\""),
                "config.toml keeps its theme"
            );
            assert_eq!(app.runtime.config.theme.name, "tide", "this machine's file wins");
            assert!(app.runtime.host_theme);
            let note = format!("config.d/{host_name} picks this machine's theme and wins over config.toml");
            assert!(find(app, ctx, &note).is_none(), "no note while saving to this machine's file");
            app.client.theme_host_only = false;
            assert!(find(app, ctx, &note).is_some(), "saving to config.toml says the host file wins");
        });
    }

    #[test]
    fn picking_replaces_a_legacy_preset_and_names_inline_overrides() {
        with_app("app::themes::tests::picking_replaces_a_legacy_preset_and_names_inline_overrides", |app, ctx| {
            let main = app.runtime.config_path.clone();
            std::fs::write(&main, "# mine\n[theme]\npreset = \"midnight\" # old key\naccent = \"#00ff88\"\n").unwrap();
            app.runtime.reload_config();
            assert_eq!(app.runtime.config.theme.name, "midnight");
            click(app, ctx, "Settings");
            let note = "Set in [theme], so shown over any theme: accent";
            assert!(find(app, ctx, note).is_some(), "inline overrides are named under the list");
            click(app, ctx, "paper");
            assert_eq!(
                std::fs::read_to_string(&main).unwrap(),
                "# mine\n[theme]\naccent = \"#00ff88\"\nname = \"paper\"\n",
                "preset gives way to name, and the override stays"
            );
            assert!(app.runtime.config_error.is_none(), "{:?}", app.runtime.config_error);
            assert_eq!(app.runtime.config.theme.name, "paper");
            assert_eq!(app.runtime.palette.mode, imacs_core::theme::ThemeMode::Light);
            assert_eq!(app.runtime.palette.accent, crate::theme::parse_hex("#00ff88").unwrap(), "[theme] still wins");

            // A file that does not parse is never rewritten.
            std::fs::write(&main, "[theme\nname = 'claude'").unwrap();
            app.runtime.select_theme("midnight", false);
            assert!(app.runtime.toast.as_ref().unwrap().0.contains("must be fixed by hand"));
            assert_eq!(std::fs::read_to_string(&main).unwrap(), "[theme\nname = 'claude'");
        });
    }
}
