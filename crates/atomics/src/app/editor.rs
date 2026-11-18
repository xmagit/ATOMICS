use super::*;

const MAX_EDITORS_PER_WORKTREE: usize = 8;
const TASK_EDITOR_DISABLED: &str =
    "The host editor is disabled for task worktrees. Use the Diff panel to review files.";

impl State {
    /// Shows a file in the diff panel: its hunks when changed, the whole file otherwise.
    pub(super) fn open_file(&mut self, root: &Path, path: &str) -> Result<(), String> {
        if self.panel == PanelMode::Editor {
            return self.edit(root, Some(path), None);
        }
        self.diff_open = true;
        let state = self.diffs.entry(root.to_path_buf()).or_default();
        let changed = state
            .prepared
            .as_ref()
            .is_some_and(|p| !p.diff.partial && p.diff.files.iter().any(|file| file.path() == path));
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
        Ok(())
    }

    /// Switches the panel to the editor for `root`, opening `path` at `line`.
    pub(super) fn edit(&mut self, root: &Path, path: Option<&str>, line: Option<u32>) -> Result<(), String> {
        self.panel = PanelMode::Editor;
        self.diff_open = true;
        self.edit_terminal(root, path, line)?;
        self.client_events.push(ClientEvent::Edit { root: root.to_path_buf(), path: path.map(str::to_string) });
        Ok(())
    }

    /// A live editor may have unsaved changes. Keep its PTY when showing another file.
    pub(super) fn edit_terminal(&mut self, root: &Path, path: Option<&str>, line: Option<u32>) -> Result<(), String> {
        if self.task_editor_path(root) {
            self.editor_errors.insert(root.to_owned(), TASK_EDITOR_DISABLED.into());
            return Err(TASK_EDITOR_DISABLED.into());
        }
        let file = editor_path(root, path)?;
        if self.task_editor_path(Path::new(&file)) {
            self.editor_errors.insert(root.to_owned(), TASK_EDITOR_DISABLED.into());
            return Err(TASK_EDITOR_DISABLED.into());
        }
        let current = self.editor_files.get(root).map(String::as_str);
        if let Some(editor) = self.editors.get(root).filter(|t| !t.exited())
            && (path.is_none() || current == path)
        {
            if line.is_some() {
                editor.type_steps(self.config.editor.open_steps(line, false), Duration::ZERO);
            }
            return Ok(());
        }

        let key = (root.to_owned(), path.map(str::to_owned));
        let editor = match self.parked_editors.remove(&key).filter(|t| !t.exited()) {
            Some(editor) => {
                if line.is_some() {
                    editor.type_steps(self.config.editor.open_steps(line, false), Duration::ZERO);
                }
                editor
            }
            None => match self.start_editor(root, &file, line) {
                Ok(editor) => editor,
                Err(error) => {
                    self.editor_errors.insert(root.to_owned(), error.clone());
                    return Err(error);
                }
            },
        };
        self.editor_errors.remove(root);
        let previous = self.editor_files.remove(root);
        if let Some(old) = self.editors.insert(root.to_owned(), editor).filter(|t| !t.exited()) {
            self.parked_editors.insert((root.to_owned(), previous), old);
        }
        if let Some(path) = path {
            self.editor_files.insert(root.to_owned(), path.to_owned());
        }
        Ok(())
    }

    fn task_editor_path(&self, path: &Path) -> bool {
        let below = |root: &Path| paths::at_or_below(path, root);
        git::pinned(path).is_some()
            || self.plans.iter().any(|plan| {
                plan.tasks.iter().any(|task| {
                    if let Some(root) = &task.worktree {
                        return below(root);
                    }
                    let repo = task.repo.as_deref().map(paths::expand).or_else(|| plan.project.clone());
                    repo.zip(task.branch.as_deref()).is_some_and(|(repo, branch)| {
                        below(&git::plan_task_worktree_path(&self.config.worktree_path, &repo, plan, task, branch))
                    })
                })
            })
    }

    fn start_editor(&mut self, root: &Path, file: &str, line: Option<u32>) -> Result<Terminal, String> {
        let parked = self.parked_editors.iter().filter(|((dir, _), editor)| dir == root && !editor.exited()).count();
        let active = usize::from(self.editors.get(root).is_some_and(|editor| !editor.exited()));
        if parked + active >= MAX_EDITORS_PER_WORKTREE {
            return Err(format!(
                "Quit an editor to open another file. This worktree has {MAX_EDITORS_PER_WORKTREE} open editors."
            ));
        }
        let Some((program, rest)) = self.config.editor.command.split_first() else {
            return Err("No editor configured ([editor] command)".into());
        };
        let executable = editor_executable(program, root, crate::setup::find_program)?;
        let mut args = rest.to_vec();
        match Path::new(program).file_name().and_then(|name| name.to_str()) {
            // Without -w Helix selects a directory argument as its workspace before reading .helix.
            Some("hx" | "helix") => args.extend(["-w".into(), ".".into()]),
            Some("nvim" | "vim") => args.extend(
                ["-u", "NONE", "-i", "NONE", "--noplugin", "--cmd", "set noexrc nomodeline"].map(str::to_owned),
            ),
            _ => {
                return Err(format!(
                    "Choose hx, helix, vim or nvim in [editor] command to open worktree files (configured: {program})"
                ));
            }
        }
        args.extend(["--".into(), file.to_owned()]);
        if self.editor_workspace.is_none() {
            self.editor_workspace =
                Some(editor_workspace().map_err(|e| format!("Could not prepare the editor workspace: {e}"))?);
        }
        let cwd = self.editor_workspace.as_ref().unwrap().path();
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "atomics")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        // Helix discovers Git from the file as well as its cwd. Its native diff provider
        // applies repository-configured filters while reading the base.
        env.insert("GIT_DIR".into(), "/dev/null".into());
        env.insert("COLORFGBG".into(), self.theme.colorfgbg().into());
        if let Some(shell_path) = crate::setup::login_path_now().as_ref() {
            env.insert("PATH".into(), shell_path.clone());
        }
        let scrollback = self.config.terminal.scrollback;
        let editor =
            Terminal::spawn(self.wake.clone(), self.theme.terminal_colors(), executable, args, cwd, env, scrollback)
                .map_err(|err| format!("Could not start the editor: {err:#}"))?;
        editor.type_steps(self.config.editor.open_steps(line, true), Duration::from_millis(900));
        Ok(editor)
    }
}

fn editor_workspace() -> std::io::Result<tempfile::TempDir> {
    let workspace = tempfile::Builder::new().prefix("atomics-editor-").tempdir()?;
    let helix = workspace.path().join(".helix");
    std::fs::create_dir(&helix)?;
    // Language servers can load project-specific programs. The editing workspace owns
    // these settings, even when the opened file has a project or .editorconfig beside it.
    std::fs::write(helix.join("config.toml"), "[editor]\neditor-config = false\n[editor.lsp]\nenable = false\n")?;
    Ok(workspace)
}

fn editor_path(root: &Path, path: Option<&str>) -> Result<String, String> {
    use std::path::Component;
    if path.is_some_and(|path| path.chars().any(char::is_control)) {
        return Err("Cannot open an editor path containing control characters".into());
    }
    let root = root.canonicalize().map_err(|e| format!("Cannot open the worktree: {e}"))?;
    let mut file = root.clone();
    let mut components = Path::new(path.unwrap_or("")).components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::CurDir => continue,
            Component::Normal(name) => file.push(name),
            _ => return Err("Name a file inside the worktree".into()),
        }
        match std::fs::symlink_metadata(&file) {
            Ok(_) => {
                file = file.canonicalize().map_err(|e| format!("Cannot resolve the editor path: {e}"))?;
                if !file.starts_with(&root) {
                    return Err("Name a file inside the worktree".into());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && components.peek().is_none() => {}
            Err(e) => return Err(format!("Cannot open the editor path: {e}")),
        }
    }
    let file = file.to_str().ok_or("The editor path is not valid UTF-8")?;
    if file.chars().any(char::is_control) {
        return Err("Cannot open an editor path containing control characters".into());
    }
    Ok(file.to_owned())
}

/// Only the host selects the executable. A relative executable path would depend on
/// the current checkout, even when it came from an entry in PATH.
fn editor_executable(program: &str, root: &Path, find: impl Fn(&str) -> Option<PathBuf>) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let candidate = if program.contains('/') {
        PathBuf::from(program)
    } else {
        crate::setup::editor_on_path(program, find)
            .ok_or_else(|| format!("{program} is not installed on this host ([editor] command)"))?
    };
    if !candidate.is_absolute() {
        return Err(format!("Use a host executable name or an absolute path in [editor] command: {program}"));
    }
    let target = candidate
        .canonicalize()
        .ok()
        .filter(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .ok_or_else(|| format!("{program} is not installed on this host ([editor] command)"))?;
    if paths::at_or_below(&candidate, root) || paths::at_or_below(&target, root) {
        return Err("The configured editor must be installed outside the worktree".into());
    }
    Ok(target.to_string_lossy().into_owned())
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
    use super::super::tests::{fixture_editor, repository, with_app, with_app_env};
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
    fn editor_targets_stay_inside_the_worktree() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("outside"), "private").unwrap();
        std::os::unix::fs::symlink("../outside", root.join("linked")).unwrap();
        std::os::unix::fs::symlink("..", root.join("ancestor")).unwrap();
        std::os::unix::fs::symlink("../missing", root.join("dangling")).unwrap();
        for path in ["linked", "ancestor/outside", "ancestor/new", "dangling"] {
            assert!(editor_path(&root, Some(path)).is_err(), "{path}");
        }
        std::fs::create_dir(root.join("sub")).unwrap();
        std::os::unix::fs::symlink("sub", root.join("alias")).unwrap();
        std::os::unix::fs::symlink("sub", root.join("control\r")).unwrap();
        assert!(editor_path(&root, Some("control\r")).is_err());
        let target = editor_path(&root, Some("alias/new")).unwrap();
        std::fs::remove_file(root.join("alias")).unwrap();
        std::os::unix::fs::symlink("..", root.join("alias")).unwrap();
        std::fs::write(&target, "saved").unwrap();
        assert_eq!(std::fs::read(root.join("sub/new")).unwrap(), b"saved");
        assert!(!dir.path().join("new").exists());
    }

    #[test]
    fn a_task_worktree_never_hands_a_live_path_to_the_host_editor() {
        with_app("app::editor::tests::a_task_worktree_never_hands_a_live_path_to_the_host_editor", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path().join("task");
            let parent = root.join("src");
            std::fs::create_dir_all(&parent).unwrap();
            std::fs::write(parent.join("file"), "task contents").unwrap();
            app.runtime.plans = vec![Plan {
                tasks: vec![plan::Task { worktree: Some(root.clone()), ..Default::default() }],
                ..Default::default()
            }];
            app.runtime.config.editor.command = fixture_editor("exec cat");

            assert!(app.runtime.edit(&root, Some("src/file"), None).unwrap_err().contains("task worktree"));
            let outside = dir.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("file"), "outside contents").unwrap();
            std::fs::rename(&parent, root.join("old-src")).unwrap();
            std::os::unix::fs::symlink(&outside, &parent).unwrap();
            assert!(app.runtime.edit_terminal(&root, Some("src/file"), None).is_err());
            assert!(app.runtime.edit_terminal(&root, None, None).is_err());
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert!(app.runtime.edit_terminal(&alias, Some("old-src/file"), None).is_err());
            assert!(app.runtime.editors.is_empty());
            assert!(app.runtime.parked_editors.is_empty());
            assert_eq!(std::fs::read_to_string(outside.join("file")).unwrap(), "outside contents");
        });
    }

    #[test]
    fn a_missing_editor_program_names_itself_and_the_setting() {
        let root = Path::new("/nonexistent-worktree");
        assert_eq!(
            editor_executable("hx", root, |_| None),
            Err("hx is not installed on this host ([editor] command)".into())
        );
        let expected = Path::new("/bin/sh").canonicalize().unwrap().display().to_string();
        assert_eq!(
            editor_executable("hx", root, |name| (name == "helix").then(|| PathBuf::from("/bin/sh"))),
            Ok(expected.clone())
        );
        assert!(editor_executable("./bin/editor", root, |_| None).is_err());
        assert!(editor_executable("hx", root, |_| Some("bin/hx".into())).is_err());
        assert!(editor_executable("/bin/sh", Path::new("/bin"), |_| None).is_err());
        assert_eq!(editor_executable("/bin/sh", root, |_| None), Ok(expected));
    }

    #[test]
    fn editor_opens_a_literal_argument_outside_the_project_workspace() {
        with_app("app::editor::tests::editor_opens_a_literal_argument_outside_the_project_workspace", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path().join("project");
            std::fs::create_dir(&root).unwrap();
            let recorder = dir.path().join("hx");
            crate::test_support::write_program(
                &recorder,
                r#"#!/bin/sh
for last do :; done
printf '%s\n' "$PWD" "$last" > "$1"
exec cat
"#,
            );
            let log = dir.path().join("args");
            app.runtime.config.editor.command = vec![recorder.display().to_string(), log.display().to_string()];
            let name = "-notes {line} 'two words'.txt";
            app.runtime.edit_terminal(&root, Some(name), Some(4)).unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
            let recorded = loop {
                if let Ok(recorded) = std::fs::read_to_string(&log)
                    && recorded.ends_with('\n')
                {
                    break recorded;
                }
                assert!(Instant::now() < deadline, "editor did not record its arguments");
                std::thread::sleep(Duration::from_millis(5));
            };
            let mut lines = recorded.lines();
            assert!(!paths::at_or_below(Path::new(lines.next().unwrap()), &root));
            assert_eq!(lines.next(), root.canonicalize().unwrap().join(name).to_str());
            assert!(lines.next().is_none());

            let first = app.runtime.editors[&root].process_id();
            app.runtime.sessions.push(super::super::tests::session(
                "worker",
                AgentKind::Shell,
                AgentStatus::Idle,
                &root,
            ));
            let response =
                super::super::tests::request(app, json!({"cmd":"edit", "session":"worker", "path":"invalid\nname"}));
            assert!(!response.ok);
            assert!(response.error.unwrap().contains("control characters"));
            app.runtime.edit_terminal(&root, Some("second file"), None).unwrap();
            assert_ne!(app.runtime.editors[&root].process_id(), first);
            app.runtime.edit_terminal(&root, Some(name), None).unwrap();
            assert_eq!(app.runtime.editors[&root].process_id(), first);
            assert_eq!(app.runtime.editor_files[&root], name);

            for n in 2..MAX_EDITORS_PER_WORKTREE {
                app.runtime.edit_terminal(&root, Some(&format!("file-{n}")), None).unwrap();
            }
            assert!(app.runtime.edit_terminal(&root, Some("one more"), None).is_err());
            app.runtime.edit_terminal(&root, Some(name), None).unwrap();
            assert_eq!(app.runtime.editors[&root].process_id(), first);
        });
    }

    #[test]
    fn editor_paths_refuse_control_characters_and_never_read_as_options() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path();
        assert!(editor_path(root, Some("a\rb")).unwrap_err().contains("control characters"));
        assert!(editor_path(root, Some("a\x1b.rs")).is_err());
        assert!(editor_path(Path::new("/a\nb"), None).is_err());
        assert!(editor_path(root, Some("../other")).is_err());
        assert!(editor_path(root, Some("/other")).is_err());
        assert_eq!(
            editor_path(root, Some("-name.rs")),
            Ok(root.canonicalize().unwrap().join("-name.rs").display().to_string())
        );
    }

    #[test]
    fn a_failed_start_reaches_the_editor_projection_until_the_editor_starts() {
        with_app(
            "app::editor::tests::a_failed_start_reaches_the_editor_projection_until_the_editor_starts",
            |app, _| {
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
                assert!(
                    app.runtime.edit_terminal(root, Some("a\r.rs"), None).unwrap_err().contains("control characters")
                );
                app.runtime.config.editor.command = fixture_editor("exec sleep 5");
                app.runtime.config.editor.startup_keys.clear();
                app.runtime.config.editor.open_keys.clear();
                app.runtime.edit_terminal(root, None, None).unwrap();
                let infos = app.runtime.editor_infos();
                assert_eq!(infos.len(), 1);
                assert_eq!(infos[0].error, None);
            },
        );
    }

    #[test]
    fn failed_restart_projects_the_error_beside_the_exited_editor() {
        with_app("app::editor::tests::failed_restart_projects_the_error_beside_the_exited_editor", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path();
            app.runtime.config.editor.command = fixture_editor("exit 0");
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
    fn helix_opens_files_and_folders_without_loading_project_settings() {
        with_app_env(
            "app::editor::tests::helix_opens_files_and_folders_without_loading_project_settings",
            |_| {
                crate::setup::editor_on_path("hx", |name| {
                    crate::setup::find_on_path(name, &std::env::var("PATH").unwrap_or_default())
                })
                .map(|hx| ("ATOMICS_TEST_HELIX", hx.into_os_string()))
                .into_iter()
                .collect()
            },
            |app, _ctx| {
                let Some(helix) = std::env::var_os("ATOMICS_TEST_HELIX") else {
                    eprintln!("Helix is not installed; skipping the native editor test");
                    return;
                };
                let (dir, repo) = repository();
                std::fs::write(repo.join("sample.txt"), "first\noriginal\nlast\n").unwrap();
                git::git(&repo, &["add", "sample.txt"]).unwrap();
                git::git(&repo, &["commit", "-qm", "tracked sample"]).unwrap();
                let root = dir.path().join("linked");
                git::git(&repo, &["worktree", "add", "--detach", root.to_str().unwrap()]).unwrap();
                std::fs::write(root.join("sample.txt"), "first\nunique edited buffer contents\nlast\n").unwrap();
                std::fs::create_dir(root.join(".helix")).unwrap();
                // Invalid settings would stop startup at Helix's configuration prompt.
                for config in ["config.toml", "languages.toml"] {
                    std::fs::write(root.join(".helix").join(config), "[").unwrap();
                }
                app.runtime.config.editor.startup_keys.clear();
                let log = dir.path().join("helix.log");
                app.runtime.config.editor.command =
                    vec![helix.to_string_lossy().into_owned(), "--log".into(), log.to_string_lossy().into_owned()];
                for path in [None, Some("sample.txt")] {
                    app.runtime.edit_terminal(&root, path, None).unwrap();
                    let terminal = &app.runtime.editors[&root];
                    let started = Instant::now();
                    let deadline = started + crate::test_support::scaled_timeout(Duration::from_secs(6));
                    loop {
                        let screen = terminal.screen_text(24);
                        if started.elapsed() >= Duration::from_secs(1)
                            && screen.contains(if path.is_none() {
                                "sample.txt"
                            } else {
                                "unique edited buffer contents"
                            })
                        {
                            assert!(!screen.contains('▍'), "Helix discovered the worktree's Git data: {screen}");
                            break;
                        }
                        assert!(Instant::now() < deadline, "Helix did not open {path:?}: {screen}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            },
        );
    }

    #[test]
    fn restarted_editor_receives_geometry_at_an_unchanged_panel_size() {
        with_app("app::editor::tests::restarted_editor_receives_geometry_at_an_unchanged_panel_size", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path();
            app.runtime.config.editor.command = fixture_editor(
                "stty -echo; printf 'ready\\n'; while read -r input; do case \"$input\" in quit) exit;; *) stty size;; esac; done",
            );
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
            app.runtime.edit(root, None, None).unwrap();
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
            app.runtime.edit(root, None, None).unwrap();
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
