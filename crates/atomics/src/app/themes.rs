//! What Settings' theme picker offers and does: every built-in theme and theme file, each with a
//! strip of its own colors. A pick shows the theme at once and saves `[theme] name` the way an
//! applied config change is saved: comments kept, a backup, an atomic write, then a reload.
use super::*;
use atomics_core::config::ThemeConfig;
use atomics_core::projection::{ThemeChoice, ThemesInfo};
use atomics_core::selfconfig;

/// The built-in themes, then the theme files in `dir`. A file named like a built-in theme takes
/// its place, since that is the theme the name now shows. `selected` is the name in `[theme]`.
fn theme_choices(dir: &Path, selected: &str) -> Vec<ThemeChoice> {
    let files = atomics_core::theme::file_names(dir);
    let builtin: Vec<&str> = atomics_core::theme::builtin_names().collect();
    let names = builtin
        .iter()
        .map(|name| (name.to_string(), files.iter().any(|file| file == name)))
        .chain(files.iter().filter(|file| !builtin.contains(&file.as_str())).map(|file| (file.clone(), true)));
    names
        .map(|(name, file)| {
            let p = Theme::load(&ThemeConfig { name: name.clone(), ..ThemeConfig::default() }, dir).0.palette();
            let swatches = [p.background, p.panel, p.foreground, p.accent, p.ansi[1], p.ansi[2], p.ansi[4]]
                .map(|color| format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b()))
                .into();
            let selected = name == selected || (!file && name.eq_ignore_ascii_case(selected));
            ThemeChoice { name, selected, file, swatches }
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
    fn displayed_theme_name(&self) -> &str {
        if let Some(name) = &self.store.pinned_theme {
            return name;
        }
        if !self.theme_preview {
            match self.theme_chief {
                Some(ChiefAgent::Codex) => return "codex",
                Some(ChiefAgent::Claude) => return "claude",
                _ => {}
            }
        }
        self.config.theme.name.trim()
    }

    pub(super) fn follow_chief_theme(&mut self) -> bool {
        let provider = self
            .sessions
            .iter()
            .find(|s| s.current_chief() && !self.is_chief_replacement(&s.id))
            .map(super::chief_provider::session_provider);
        if self.theme_chief == provider {
            return false;
        }
        self.theme_chief = provider;
        self.theme_preview = false;
        if self.store.pinned_theme.is_some() {
            return false;
        }
        self.apply_display_theme();
        true
    }

    pub(super) fn apply_display_theme(&mut self) {
        let mut config = self.config.theme.clone();
        config.name = self.displayed_theme_name().into();
        let (theme, errors) = Theme::load(&config, &atomics_core::theme::themes_dir(&self.config_path));
        self.theme_error = theme_error(&errors);
        self.management_config.take();
        if *self.theme != theme {
            if self.theme.syntax != theme.syntax {
                self.highlighter = Arc::new(Highlighter::new(&theme.syntax));
            }
            self.theme = Arc::new(theme);
            let colors = self.theme.terminal_colors();
            for terminal in self.sessions.iter().filter_map(|s| s.terminal.as_ref()).chain(self.editors.values()) {
                terminal.set_colors(colors.clone());
            }
            for state in self.diffs.values_mut() {
                state.mark_dirty();
            }
        }
        self.refresh_themes();
        self.client_events.push(ClientEvent::ConfigChanged);
    }

    pub(super) fn pin_theme(&mut self, pinned: bool) -> Result<(), String> {
        let next = pinned.then(|| self.displayed_theme_name().to_owned());
        let old = std::mem::replace(&mut self.store.pinned_theme, next);
        if let Err(error) = self.store.save_later(&paths::store_file()) {
            self.store.pinned_theme = old;
            return Err(error.to_string());
        }
        self.theme_preview = false;
        self.apply_display_theme();
        Ok(())
    }

    /// The picker's themes, and whether this machine's file picks one; read when the config
    /// or a theme file changes, never per frame.
    pub(super) fn refresh_themes(&mut self) {
        let [_, host_file] = Config::source_paths(&self.config_path);
        let host = std::fs::read_to_string(&host_file).unwrap_or_default();
        self.host_theme = selfconfig::picks_theme(&host);
        let dir = atomics_core::theme::themes_dir(&self.config_path);
        let cleaner = self.config_cleaner(&self.config_layers(None));
        self.themes = ThemesInfo {
            choices: theme_choices(&dir, self.displayed_theme_name()),
            pinned: self.store.pinned_theme.is_some(),
            host_file: format!(
                "config.d/{}",
                host_file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
            ),
            host_picks: self.host_theme,
            inline: self.config.theme.tokens.set_keys().into_iter().map(str::to_string).collect(),
            error: self.theme_error.as_deref().map(|error| cleaner.text(error)),
        };
    }

    /// The user picked a theme: save its name to `config.toml`, or to this machine's file, and
    /// show it at once. The user's own click, so no proposal.
    pub(super) fn select_theme(&mut self, name: &str, host: bool) -> Result<(), String> {
        if !self.themes.choices.iter().any(|choice| choice.name == name) {
            return Err(format!("There is no theme named {name}"));
        }
        let [main, host_file] = Config::source_paths(&self.config_path);
        let file = if host { host_file } else { main };
        let changed = write_theme_name(&file, name, atomics_core::store::now())
            .map_err(|err| format!("Theme not saved: {err}"))?;
        if host || !self.host_theme {
            self.theme_preview = true;
            if self.store.pinned_theme.is_some() {
                let previous = self.store.pinned_theme.replace(name.into());
                if let Err(error) = self.store.save_later(&paths::store_file()) {
                    self.store.pinned_theme = previous;
                    return Err(error.to_string());
                }
            }
        }
        if changed {
            self.reload_config();
        } else {
            self.apply_display_theme();
        }
        let shown = paths::display(&file);
        self.toast(if !host && self.host_theme {
            format!("Theme {name} saved to {shown}, but this machine's config.d file picks its own")
        } else {
            format!("Theme {name} saved to {shown}")
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::with_state;
    use super::*;
    use atomics_core::ops::HostAction;

    #[test]
    fn chief_changes_follow_the_provider_while_a_pin_survives_switches_and_restart() {
        with_state(
            "app::themes::tests::chief_changes_follow_the_provider_while_a_pin_survives_switches_and_restart",
            |mut state, _| {
                let mut chief =
                    super::super::tests::session("chief", AgentKind::Generic, AgentStatus::Starting, &state.launch_dir);
                chief.chief = true;
                chief.agent = "codex".into();
                state.sessions.push(chief);
                assert!(state.follow_chief_theme());
                assert_eq!(state.displayed_theme_name(), "codex");
                assert_eq!(state.theme.tokens.accent.as_deref(), Some("#b399fc"));
                state.select_theme("paper", false).unwrap();
                assert!(!state.follow_chief_theme());
                assert_eq!(state.displayed_theme_name(), "paper");
                state.sessions[0].agent = "claude".into();
                assert!(state.follow_chief_theme());
                assert_eq!(state.displayed_theme_name(), "claude");
                state.pin_theme(true).unwrap();
                state.sessions[0].agent = "codex".into();
                assert!(!state.follow_chief_theme());
                assert_eq!(state.displayed_theme_name(), "claude");
                state.select_theme("midnight", false).unwrap();
                atomics_core::store::flush_writes();
                assert_eq!(Store::load(&paths::store_file()).pinned_theme.as_deref(), Some("midnight"));
                assert!(state.themes.pinned && state.themes.choices.iter().any(|t| t.selected && t.name == "midnight"));
                state.pin_theme(false).unwrap();
                assert_eq!(state.displayed_theme_name(), "codex");
                assert!(!state.themes.pinned);
            },
        );
    }

    #[test]
    fn selecting_a_theme_keeps_comments_and_backs_up_the_file() {
        with_state("app::themes::tests::selecting_a_theme_keeps_comments_and_backs_up_the_file", |mut state, _| {
            let file = state.config_path.clone();
            let before = "# theme\n[theme]\npreset = \"midnight\" # old key\naccent = \"#00ff88\"\n";
            std::fs::write(&file, before).unwrap();
            state.reload_config();
            state.select_theme("paper", false).unwrap();
            assert_eq!(state.config.theme.name, "paper");
            assert_eq!(state.theme.palette().accent, atomics_ui::theme::parse_hex("#00ff88").unwrap());
            let written = std::fs::read_to_string(&file).unwrap();
            assert!(written.starts_with("# theme") && written.contains("name = \"paper\""), "{written}");
            assert!(!written.contains("preset"));
            let backups: Vec<_> = std::fs::read_dir(file.parent().unwrap())
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "bak"))
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), before);
            std::fs::write(&file, "[theme\nname = 'claude'").unwrap();
            assert!(state.select_theme("midnight", false).unwrap_err().contains("Theme not saved"));
            assert_eq!(std::fs::read_to_string(file).unwrap(), "[theme\nname = 'claude'");
        });
    }

    #[test]
    fn two_picks_in_a_row_leave_the_second_theme_with_no_proposal() {
        with_state("app::themes::tests::two_picks_in_a_row_leave_the_second_theme_with_no_proposal", |mut state, _| {
            std::fs::write(&state.config_path, "[layout]\nmin_tile_width = 407.0\n").unwrap();
            state.reload_config();
            let pick = |name: &str| HostAction::SelectTheme { name: name.into(), host: false };
            for name in ["paper", "midnight"] {
                state.manage(pick(name)).unwrap();
                assert_eq!(state.config.theme.name, name, "it shows at once");
                assert!(state.proposals.is_empty(), "the user's own pick is not proposed");
                let chosen: Vec<_> =
                    state.management_info().themes.choices.into_iter().filter(|c| c.selected).map(|c| c.name).collect();
                assert_eq!(chosen, [name], "the picker follows");
            }
            let written = std::fs::read_to_string(&state.config_path).unwrap();
            assert!(written.contains("name = \"midnight\"") && !written.contains("paper"), "{written}");
            assert!(written.contains("min_tile_width = 407.0"), "the rest stays: {written}");

            let refused = state.manage(pick("../outside")).unwrap_err();
            assert!(refused.message.contains("no theme"), "{refused:?}");
            assert_eq!(state.config.theme.name, "midnight");
        });
    }

    #[test]
    fn a_pick_can_go_to_this_machines_file_and_the_picker_says_who_wins() {
        with_state(
            "app::themes::tests::a_pick_can_go_to_this_machines_file_and_the_picker_says_who_wins",
            |mut state, _| {
                let [main, host] = Config::source_paths(&state.config_path);
                std::fs::write(&main, "[theme]\nname = 'paper'\n").unwrap();
                state.reload_config();
                assert!(!state.management_info().themes.host_picks);
                state.manage(HostAction::SelectTheme { name: "midnight".into(), host: true }).unwrap();
                assert!(std::fs::read_to_string(&host).unwrap().contains("midnight"));
                assert!(std::fs::read_to_string(&main).unwrap().contains("paper"), "config.toml keeps its own");
                let themes = state.management_info().themes;
                assert!(themes.host_picks);
                assert!(themes.host_file.starts_with("config.d/") && themes.host_file.ends_with(".toml"));
                assert_eq!(state.config.theme.name, "midnight");
            },
        );
    }

    #[test]
    fn the_picker_lists_theme_files_with_their_own_colors() {
        with_state("app::themes::tests::the_picker_lists_theme_files_with_their_own_colors", |mut state, _| {
            let themes = atomics_core::theme::themes_dir(&state.config_path);
            std::fs::create_dir_all(&themes).unwrap();
            std::fs::write(themes.join("dusk.toml"), "extends = \"midnight\"\naccent = \"#e0a458\"\n").unwrap();
            std::fs::write(&state.config_path, "[theme]\nname = 'dusk'\naccent = '#123456'\n").unwrap();
            state.reload_config();
            let themes = state.management_info().themes;
            let dusk = themes.choices.iter().find(|c| c.name == "dusk").expect("the file is offered");
            assert!(dusk.file && dusk.selected);
            assert_eq!(dusk.swatches.len(), 7);
            assert_eq!(dusk.swatches[3], "#e0a458", "its own accent, not the one [theme] sets over it");
            assert_eq!(themes.inline, ["accent"]);
            let midnight = themes.choices.iter().find(|c| c.name == "midnight").unwrap();
            assert!(!midnight.file && !midnight.selected);
        });
    }
}
