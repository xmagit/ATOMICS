use super::*;

impl State {
    /// Shows a file in the diff panel: its hunks when changed, the whole file otherwise.
    pub(super) fn open_file(&mut self, root: &Path, path: &str) {
        if self.panel == PanelMode::Editor {
            self.edit(root, Some(path), None);
            return;
        }
        self.diff_open = true;
        let state = self.diffs.entry(root.to_path_buf()).or_default();
        let changed = state.prepared.as_ref().is_some_and(|p| p.diff.files.iter().any(|file| file.path() == path));
        state.viewing = (!changed).then(|| path.to_string());
        state.file = None;
        state.file_error = None;
        if !changed {
            self.worker.request_file(root.to_path_buf(), path.to_string(), self.highlighter.clone());
        }
        self.client_events.push(ClientEvent::OpenFile {
            root: root.to_path_buf(),
            path: path.to_string(),
            whole_file: !changed,
        });
    }

    /// Switches the panel to the editor for `root`, opening `path` at `line`. One
    /// editor runs per worktree and keeps running when you switch back to the diff.
    pub(super) fn edit(&mut self, root: &Path, path: Option<&str>, line: Option<u32>) {
        self.panel = PanelMode::Editor;
        self.diff_open = true;
        self.client_events.push(ClientEvent::Edit { root: root.to_path_buf(), path: path.map(str::to_string) });
        if let Err(error) = self.edit_terminal(root, path, line) {
            self.toast(error);
        }
    }

    /// Host operation: starts or types into the PTY without changing any client's view.
    pub(super) fn edit_terminal(&mut self, root: &Path, path: Option<&str>, line: Option<u32>) -> Result<(), String> {
        let typed = path.map(editor_path).transpose()?;
        if let Some(path) = path {
            self.editor_files.insert(root.to_path_buf(), path.to_string());
        }
        if let Some(editor) = self.editors.get(root).filter(|e| !e.exited()) {
            if let Some(typed) = &typed {
                editor.type_steps(self.config.editor.open_steps(typed, line, false), Duration::ZERO);
            }
            return Ok(());
        }
        let started = self.start_editor(root, typed.as_deref(), line);
        match &started {
            Ok(()) => self.editor_errors.remove(root),
            Err(error) => self.editor_errors.insert(root.to_path_buf(), error.clone()),
        };
        started
    }

    fn start_editor(&mut self, root: &Path, path: Option<&str>, line: Option<u32>) -> Result<(), String> {
        let Some((program, rest)) = self.config.editor.command.split_first() else {
            return Err("No editor configured ([editor] command)".into());
        };
        // The PATH the editor gets below is the one find_program looks on.
        let program = editor_executable(program, root, crate::setup::find_program)?;
        let mut args = rest.to_vec();
        args.push(path.unwrap_or(".").to_string());
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "atomics")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        env.insert("COLORFGBG".into(), self.theme.colorfgbg().into());
        if let Some(shell_path) = crate::setup::login_path_now().as_ref() {
            env.insert("PATH".into(), shell_path.clone());
        }
        let scrollback = self.config.terminal.scrollback;
        match Terminal::spawn(
            self.wake.clone(),
            self.theme.terminal_colors(),
            program,
            args,
            root,
            env,
            scrollback,
        ) {
            Ok(editor) => {
                // The file is already open; only the theme and the jump to the line are left.
                editor.type_steps(
                    self.config.editor.open_steps(path.unwrap_or("."), line, true),
                    Duration::from_millis(900),
                );
                self.editors.insert(root.to_path_buf(), editor);
                Ok(())
            }
            Err(err) => Err(format!("Could not start the editor: {err:#}")),
        }
    }
}

/// The path as the editor receives it, on its command line and typed as keystrokes. Control characters
/// would type keys of their own (Escape, Enter), so a path with any is refused; a path that starts with
/// `-` would read as an option, so it gets a `./` in front.
fn editor_path(path: &str) -> Result<String, String> {
    if path.chars().any(char::is_control) {
        return Err(format!("Cannot open {path:?} in the editor: the path contains control characters"));
    }
    Ok(if path.starts_with('-') { format!("./{path}") } else { path.to_string() })
}

/// Resolves the editor program the way the PTY will start it: a path with a `/` as it stands (relative
/// to the worktree), any other name on `find`'s PATH, with Helix's other name for `hx`. When it is
/// missing, says which program and which setting.
fn editor_executable(program: &str, root: &Path, find: impl Fn(&str) -> Option<PathBuf>) -> Result<String, String> {
    let found = if program.contains('/') {
        use std::os::unix::fs::PermissionsExt;
        let candidate = root.join(program);
        candidate
            .metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .then(|| program.to_string())
    } else {
        crate::setup::editor_on_path(program, find).map(|path| path.to_string_lossy().into_owned())
    };
    found.ok_or_else(|| format!("{program} is not installed on this host ([editor] command)"))
}

pub(super) fn save_png(path: &Path, image: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    writer.write_image_data(&bytes)?;
    Ok(())
}

impl Desktop<'_> {
    #[cfg(test)]
    pub(super) fn editor_panel(&mut self, ui: &mut egui::Ui, rect: Rect, root: &Path) {
        self.draw_terminals(|backend, client, terminals| {
            atomics_ui::editor::draw(ui, rect, root, backend, client, terminals)
        });
    }

    pub(super) fn quick_open_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.client.quick_open.take() else { return };
        let Some(root) = self.runtime.diff_target() else { return };
        let Some(prepared) = self.client.diff_views.get(&root).and_then(|s| s.prepared.clone()) else {
            self.client.quick_open = Some(dialog);
            return;
        };
        let mut all = prepared.all_files.to_vec();
        all.extend(prepared.diff.files.iter().map(|f| Arc::from(f.path())));
        all.sort();
        all.dedup();
        let changed: HashSet<&str> = prepared.diff.files.iter().map(|f| f.path()).collect();
        let p = self.palette();
        let mut keep = true;
        let mut open: Option<String> = None;
        egui::Window::new("Go to file")
            .title_bar(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, vec2(0.0, 70.0))
            .fixed_size(vec2(620.0, 0.0))
            .show(ctx, |ui| {
                let matches = atomics_core::files::best_matches(&dialog.query, &all, 14);
                let (down, up, enter, escape) = ui.input_mut(|i| {
                    (
                        i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                        i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                        i.key_pressed(Key::Enter),
                        i.key_pressed(Key::Escape),
                    )
                });
                if down {
                    dialog.selected = (dialog.selected + 1).min(matches.len().saturating_sub(1));
                }
                if up {
                    dialog.selected = dialog.selected.saturating_sub(1);
                }
                let before = dialog.query.clone();
                let response = ui.add(
                    atomics_ui::controls::single_line(&mut dialog.query)
                        .hint_text("Go to file · ↑↓ to pick · Enter opens · Esc closes")
                        .desired_width(f32::INFINITY)
                        .font(p.ui(15.0)),
                );
                if dialog.query != before {
                    dialog.selected = 0;
                }
                if std::mem::take(&mut dialog.focus) {
                    response.request_focus();
                }
                ui.add_space(4.0);
                for (i, path) in matches.iter().enumerate() {
                    let (dir, name) = match path.rfind('/') {
                        Some(cut) => (&path[..=cut], &path[cut + 1..]),
                        None => ("", *path),
                    };
                    let name_color = if changed.contains(path) { p.warning } else { p.foreground };
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        name,
                        0.0,
                        egui::TextFormat { font_id: p.ui(14.0), color: name_color, ..Default::default() },
                    );
                    job.append(
                        &format!("   {dir}"),
                        0.0,
                        egui::TextFormat { font_id: p.ui(12.0), color: p.muted, ..Default::default() },
                    );
                    if ui.selectable_label(i == dialog.selected, job).clicked() {
                        open = Some(path.to_string());
                    }
                }
                if matches.is_empty() {
                    ui.label(RichText::new("No matching files").color(p.muted));
                }
                if enter && let Some(path) = matches.get(dialog.selected) {
                    open = Some(path.to_string());
                }
                if escape {
                    keep = false;
                }
            });
        if let Some(path) = open {
            self.send(Command::OpenFile { root: root.clone(), path });
            keep = false;
        }
        if keep {
            self.client.quick_open = Some(dialog);
        }
    }

    pub(super) fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.client.screenshot else { return };
        let now = ctx.input(|i| i.time);
        if shot.done.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if !shot.requested && now >= shot.at {
            shot.requested = true;
            // A window hidden behind others is not painted, and an unpainted window cannot be captured.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
            let (path, done, repaint) = (shot.path.clone(), shot.done.clone(), ctx.clone());
            ctx.request_screenshot(move |image| {
                if let Err(err) = save_png(&path, &image) {
                    eprintln!("ATOMICS: screenshot failed: {err:#}");
                }
                done.store(true, Ordering::Relaxed);
                repaint.request_repaint();
            });
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, with_app};
    use super::*;

    #[test]
    fn delayed_comment_edits_preserve_duplicate_and_concurrent_comments() {
        with_app("app::editor::tests::delayed_comment_edits_preserve_duplicate_and_concurrent_comments", |app, ctx| {
            let root = app.runtime.launch_dir.clone();
            let comment = steer::ReviewComment {
                path: "review.rs".into(),
                line: Some(1),
                removed: false,
                excerpt: "line".into(),
                body: "same feedback".into(),
            };
            app.runtime.diffs.entry(root.clone()).or_default().comments = vec![comment.clone(), comment.clone()];
            let before = app.runtime.diffs[&root].comments.clone();
            let after = vec![comment.clone()];
            let target = root.clone();
            app.post(move |state| state.diffs.get_mut(&target).unwrap().edit_comments(before, after));
            for _ in 0..2 {
                ctx.run_ui(egui::RawInput::default(), |_| app.sync_diffs()).drop_without_applying_deltas();
            }
            // A matching ctl comment accepted before the desktop action runs.
            app.runtime.diffs.get_mut(&root).unwrap().comments.push(comment.clone());
            app.runtime.drain_actions();
            assert_eq!(app.runtime.diffs[&root].comments, vec![comment.clone(), comment.clone()]);
            let before = app.runtime.diffs[&root].comments.clone();
            let mut after = before.clone();
            after.push(comment.clone());
            app.runtime.diffs.get_mut(&root).unwrap().edit_comments(before, after);
            assert_eq!(app.runtime.diffs[&root].comments, vec![comment.clone(), comment.clone(), comment]);
        });
    }

    #[test]
    fn a_missing_editor_program_names_itself_and_the_setting() {
        let root = Path::new("/nonexistent-worktree");
        assert_eq!(
            editor_executable("hx", root, |_| None),
            Err("hx is not installed on this host ([editor] command)".into())
        );
        assert_eq!(
            editor_executable("hx", root, |name| (name == "helix").then(|| PathBuf::from("/bin/helix"))),
            Ok("/bin/helix".into())
        );
        assert_eq!(
            editor_executable("./bin/missing-editor", root, |_| None),
            Err("./bin/missing-editor is not installed on this host ([editor] command)".into())
        );
        assert_eq!(editor_executable("/bin/sh", root, |_| None), Ok("/bin/sh".into()));
    }

    #[test]
    fn editor_paths_refuse_control_characters_and_never_read_as_options() {
        assert!(editor_path("src/a.rs\r:sh rm -rf ~\r").unwrap_err().contains("control characters"));
        assert!(editor_path("a\x1b.rs").is_err());
        assert_eq!(editor_path("-rf.rs"), Ok("./-rf.rs".into()));
        assert_eq!(editor_path("src/a.rs"), Ok("src/a.rs".into()));
    }

    #[test]
    fn a_failed_start_reaches_the_editor_projection_until_the_editor_starts() {
        with_app("app::editor::tests::a_failed_start_reaches_the_editor_projection_until_the_editor_starts", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path();
            app.runtime.config.editor.command = vec!["atomics-test-no-such-editor".into()];
            let error = app.runtime.edit_terminal(root, Some("a.rs"), None).unwrap_err();
            assert_eq!(error, "atomics-test-no-such-editor is not installed on this host ([editor] command)");
            let infos = app.runtime.editor_infos();
            assert_eq!(infos.len(), 1);
            assert!(!infos[0].running);
            assert_eq!(infos[0].error.as_deref(), Some(error.as_str()));
            // A path with control characters is refused before anything is typed or started.
            assert!(app.runtime.edit_terminal(root, Some("a\r.rs"), None).unwrap_err().contains("control characters"));
            app.runtime.config.editor.command = vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()];
            app.runtime.config.editor.startup_keys.clear();
            app.runtime.config.editor.open_keys.clear();
            app.runtime.edit_terminal(root, None, None).unwrap();
            let infos = app.runtime.editor_infos();
            assert_eq!(infos.len(), 1);
            assert_eq!(infos[0].error, None);
        });
    }

    #[test]
    fn failed_restart_projects_the_error_beside_the_exited_editor() {
        with_app("app::editor::tests::failed_restart_projects_the_error_beside_the_exited_editor", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path();
            app.runtime.config.editor.command = vec!["/bin/sh".into(), "-c".into(), "exit 0".into()];
            app.runtime.config.editor.startup_keys.clear();
            app.runtime.config.editor.open_keys.clear();
            app.runtime.edit_terminal(root, Some("a.rs"), None).unwrap();
            let terminal = &app.runtime.editors[root];
            let process = terminal.process_id();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
            while !terminal.exited() {
                assert!(Instant::now() < deadline, "editor did not exit");
                std::thread::sleep(Duration::from_millis(5));
            }
            app.runtime.config.editor.command = vec!["atomics-test-no-such-editor".into()];
            let error = app.runtime.edit_terminal(root, Some("a.rs"), None).unwrap_err();
            assert_eq!(app.runtime.editors[root].process_id(), process);
            let infos = app.runtime.editor_infos();
            assert_eq!(infos.len(), 1);
            assert!(!infos[0].running);
            assert_eq!(infos[0].error.as_deref(), Some(error.as_str()));
            assert_eq!(infos[0].file.as_deref(), Some("a.rs"));
        });
    }

    #[test]
    fn helix_diff_gutter_uses_head_in_a_linked_worktree_with_startup_theme() {
        with_app(
            "app::editor::tests::helix_diff_gutter_uses_head_in_a_linked_worktree_with_startup_theme",
            |app, _ctx| {
                let Some(helix) = crate::setup::find_program("hx").or_else(|| crate::setup::find_program("helix"))
                else {
                    eprintln!("Helix is not installed; skipping the native diff-gutter smoke test");
                    return;
                };
                let (dir, repo) = repository();
                std::fs::write(repo.join("sample.txt"), "first\noriginal\nlast\n").unwrap();
                git::git(&repo, &["add", "sample.txt"]).unwrap();
                git::git(&repo, &["commit", "-qm", "tracked sample"]).unwrap();
                let root = dir.path().join("linked");
                git::git(&repo, &["worktree", "add", "--detach", root.to_str().unwrap()]).unwrap();
                std::fs::write(root.join("sample.txt"), "first\nmodified\nlast\n").unwrap();
                let log = dir.path().join("helix.log");
                app.runtime.config.editor.command =
                    vec![helix.to_string_lossy().into_owned(), "--log".into(), log.to_string_lossy().into_owned()];
                let check = |app: &mut Desktop<'_>, path: &str, marker: bool| {
                    app.runtime.editors.remove(&root);
                    app.runtime.edit(&root, Some(path), None);
                    let terminal = &app.runtime.editors[&root];
                    // The normal startup sequence applies the theme after 900 ms, with pauses
                    // between commands. Inspect the screen after it has settled.
                    let started = Instant::now();
                    let deadline = started + crate::test_support::scaled_timeout(Duration::from_secs(5));
                    loop {
                        let screen = terminal.screen_text(24);
                        if started.elapsed() >= Duration::from_secs(2)
                            && screen.contains(path)
                            && screen.contains("modified")
                        {
                            assert_eq!(screen.contains('▍'), marker, "{screen}");
                            break;
                        }
                        assert!(Instant::now() < deadline, "Helix did not open {path}: {screen}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                };
                check(app, "sample.txt", true);
                git::git(&root, &["add", "sample.txt"]).unwrap();
                git::git(&root, &["commit", "-qm", "committed change"]).unwrap();
                check(app, "sample.txt", false);
                std::fs::write(root.join("untracked.txt"), "first\nmodified\nlast\n").unwrap();
                check(app, "untracked.txt", false);
            },
        );
    }

    #[test]
    fn restarted_editor_receives_geometry_at_an_unchanged_panel_size() {
        with_app("app::editor::tests::restarted_editor_receives_geometry_at_an_unchanged_panel_size", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path();
            app.runtime.config.editor.command = vec!["/bin/sh".into(), "-c".into(), "stty -echo; printf 'ready\\n'; while read -r input; do case \"$input\" in quit) exit;; *) stty size;; esac; done".into()];
            app.runtime.config.editor.startup_keys.clear();
            app.runtime.config.editor.open_keys.clear();
            let wait = |terminal: &Terminal, check: &dyn Fn() -> bool| {
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
                while !check() {
                    assert!(Instant::now() < deadline, "editor did not respond: {}", terminal.screen_text(40));
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            let frame = |app: &mut Desktop<'_>| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
                        ..Default::default()
                    },
                    |ui| {
                        app.editor_panel(ui, Rect::from_min_size(Pos2::ZERO, vec2(800.0, 650.0)), root);
                    },
                )
                .drop_without_applying_deltas();
            };
            app.runtime.edit(root, None, None);
            let terminal = &app.runtime.editors[root];
            wait(terminal, &|| terminal.screen_text(40).contains("ready"));
            frame(app);
            let old_id = app.runtime.editors[root].process_id();
            let terminal = &app.runtime.editors[root];
            terminal.write(b"size\r".to_vec());
            let size_line = || {
                terminal
                    .screen_text(40)
                    .lines()
                    .find(|line| {
                        line.split_whitespace().count() == 2
                            && line.split_whitespace().all(|part| part.parse::<usize>().is_ok())
                    })
                    .map(str::to_string)
            };
            wait(terminal, &|| size_line().is_some());
            let expected = size_line().unwrap();
            assert_ne!(expected.trim(), "24 80");
            terminal.write(b"quit\r".to_vec());
            wait(terminal, &|| terminal.exited());
            // Restart without a client frame or consuming either Edit event.
            app.runtime.edit(root, None, None);
            assert_ne!(app.runtime.editors[root].process_id(), old_id);
            assert_eq!(app.client.terminal_views.len(), 1);
            frame(app);
            let terminal = &app.runtime.editors[root];
            terminal.write(b"size\r".to_vec());
            wait(terminal, &|| terminal.screen_text(40).lines().any(|line| line == expected));
            assert_eq!(app.client.terminal_views.len(), 1, "the same editor view must be reused");
        });
    }
}
