use super::*;

impl State {
    /// Shows a file in the diff panel: its hunks when changed, the whole file otherwise.
    pub(super) fn open_file(&mut self, root: &Path, path: &str) {
        if self.panel == PanelMode::Editor {
            let ctx = self.ctx.clone();
            self.edit(&ctx, root, Some(path), None);
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
    pub(super) fn edit(&mut self, _ctx: &egui::Context, root: &Path, path: Option<&str>, line: Option<u32>) {
        self.panel = PanelMode::Editor;
        self.diff_open = true;
        self.client_events.push(ClientEvent::Edit { root: root.to_path_buf(), path: path.map(str::to_string) });
        if let Some(editor) = self.editors.get(root).filter(|e| !e.exited()) {
            if let Some(path) = path {
                editor.type_steps(self.config.editor.open_steps(path, line, false), Duration::ZERO);
            }
            return;
        }
        let Some((program, rest)) = self.config.editor.command.split_first() else {
            self.toast("No editor configured ([editor] command)");
            return;
        };
        let mut args = rest.to_vec();
        args.push(path.unwrap_or(".").to_string());
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "imacs")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        if let Some(shell_path) = SHELL_PATH.as_ref() {
            env.insert("PATH".into(), shell_path.clone());
        }
        let scrollback = self.config.terminal.scrollback;
        match Terminal::spawn(
            self.wake.clone(),
            self.palette.terminal_colors(),
            resolve_program(program),
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
            }
            Err(err) => self.toast(format!("Could not start the editor: {err:#}")),
        }
    }
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
    pub(super) fn diff_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        self.sync_diffs();
        if self.runtime.panel == PanelMode::Plan {
            self.plan_panel(ui, rect);
            return;
        }
        let Some(root) = self.runtime.diff_target() else { return };
        // What changed sits right next to the diff/editor, in both modes.
        let rect = if rect.width() >= 760.0 {
            let column = rect.with_max_x(rect.left() + 250.0);
            let current = match self.runtime.panel {
                PanelMode::Editor => self.client.editor_file.get(&root).cloned(),
                _ => self.client.diff_views.get(&root).and_then(|d| d.focused.clone()),
            };
            let p = self.runtime.palette.clone();
            let state = self.client.diff_views.entry(root.clone()).or_default();
            if let Some((path, line)) = diff_view::changed_files(ui, column, state, current.as_deref(), &p) {
                match self.runtime.panel {
                    PanelMode::Editor => {
                        let ctx = ui.ctx().clone();
                        self.edit(&ctx, &root, Some(&path), line);
                    }
                    _ => {
                        self.open_file(&root, &path);
                    }
                }
            }
            rect.with_min_x(column.right())
        } else {
            rect
        };
        if self.runtime.panel == PanelMode::Editor {
            self.editor_panel(ui, rect, &root);
            return;
        }
        let session = self.runtime.active_index().map(|i| &self.runtime.sessions[i]);
        let title = session
            .map(|s| s.title.clone())
            .unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        let branch = session
            .and_then(|s| s.branch().map(str::to_string))
            .or_else(|| self.runtime.worktrees.iter().find(|w| w.path == root).and_then(|w| w.branch.clone()))
            .filter(|b| *b != title);
        let can_send = self.runtime.sessions.iter().any(|s| s.root() == root && s.is_agent() && s.can_receive());
        let palette = self.runtime.palette.clone();
        let font = palette.mono(palette.fonts.diff_size.unwrap_or(self.runtime.config.font.diff_size));
        let split = self.client.diff_split.unwrap_or(self.runtime.config.layout.diff_split);
        let state = self.client.diff_views.entry(root.clone()).or_default();
        let header = Header {
            title: &title,
            branch: branch.as_deref(),
            can_send,
            split,
            editor_shortcut: &self.runtime.config.keys.toggle_editor,
        };
        let before = state.comments.clone();
        let action = diff_view::show(ui, rect, state, header, &palette, &font);
        let after = state.comments.clone();
        if before != after {
            let root = root.clone();
            self.post(move |runtime| {
                runtime.diffs.entry(root).or_default().edit_comments(before, after);
            });
        }
        match action {
            DiffAction::None => {}
            DiffAction::SendReview => {
                self.send_review(&root);
            }
            DiffAction::Refresh => self.post(move |runtime| state_refresh(runtime.diffs.entry(root).or_default())),
            DiffAction::Close => self.runtime.diff_open = false,
            DiffAction::Split(on) => {
                self.client.diff_split = Some(if on { DiffSplit::Always } else { DiffSplit::Never })
            }
            DiffAction::Edit { path, line } => {
                let ctx = ui.ctx().clone();
                self.edit(&ctx, &root, path.as_deref(), line);
            }
        }
    }

    fn editor_panel(&mut self, ui: &mut egui::Ui, rect: Rect, root: &Path) {
        let p = self.runtime.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.terminal);
        let header = rect.with_max_y(rect.top() + 30.0);
        let body = rect.with_min_y(header.bottom());
        ui.painter().rect_filled(header, 0.0, p.raised);
        ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("editor-header")
                .max_rect(header.shrink2(vec2(10.0, 3.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        child.label(RichText::new("EDITOR").color(p.accent).strong().size(12.0));
        child.label(RichText::new(&name).color(p.muted).size(12.0));
        let editor_running = self.runtime.editors.get(root).is_some_and(|e| !e.exited());
        let mut action = None;
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("×").on_hover_text("Close the panel").clicked() {
                action = Some("close");
            }
            if ui
                .small_button("diff")
                .on_hover_text(format!(
                    "Back to the diff ({})",
                    shortcut_label(ui.ctx(), &self.runtime.config.keys.toggle_editor)
                ))
                .clicked()
            {
                action = Some("diff");
            }
            if editor_running
                && !self.runtime.config.editor.inlay_hints_keys.is_empty()
                && ui.small_button("inlay hints").on_hover_text("Toggle the language server's inlay hints").clicked()
            {
                action = Some("hints");
            }
        });
        match action {
            Some("close") => self.runtime.diff_open = false,
            Some("diff") => self.runtime.panel = PanelMode::Diff,
            Some("hints") => {
                if let Some(editor) = self.runtime.editors.get(root) {
                    editor.type_steps(self.runtime.config.editor.inlay_hints_keys.clone(), Duration::ZERO);
                }
            }
            _ => {}
        }

        let metrics = self.metrics(ui);
        let id = ui.scope_id().with(("editor", root));
        self.client.editor_id = Some(id);
        match self.runtime.editors.get_mut(root).filter(|e| !e.exited()) {
            Some(editor) => {
                let response =
                    self.client.terminal_views.entry(id).or_default().show(editor, ui, body, id, &metrics, &p);
                if std::mem::take(&mut self.client.focus_editor) {
                    response.request_focus();
                }
            }
            None => {
                self.runtime.editors.remove(root);
                let mut child =
                    ui.new_child(egui::UiBuilder::new().id_salt("editor-empty").max_rect(body.shrink(20.0)));
                child.label(RichText::new("The editor is not running.").color(p.muted));
                if child.button("Open editor").clicked() {
                    let ctx = ui.ctx().clone();
                    self.edit(&ctx, root, None, None);
                }
            }
        }
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
        let p = self.runtime.palette.clone();
        let mut keep = true;
        let mut open: Option<String> = None;
        egui::Window::new("Go to file")
            .title_bar(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, vec2(0.0, 70.0))
            .fixed_size(vec2(620.0, 0.0))
            .show(ctx, |ui| {
                let matches = imacs_core::files::best_matches(&dialog.query, &all, 14);
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
                    egui::TextEdit::singleline(&mut dialog.query)
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
            self.open_file(&root, &path);
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
                    eprintln!("IMACS: screenshot failed: {err:#}");
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
    use super::super::tests::with_app;
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
    fn restarted_editor_receives_geometry_at_an_unchanged_panel_size() {
        with_app("app::editor::tests::restarted_editor_receives_geometry_at_an_unchanged_panel_size", |app, ctx| {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            app.runtime.config.editor.command = vec!["/bin/sh".into(), "-c".into(), "stty -echo; printf 'ready\\n'; while read -r input; do case \"$input\" in quit) exit;; *) stty size;; esac; done".into()];
            app.runtime.config.editor.startup_keys.clear();
            app.runtime.config.editor.open_keys.clear();
            let wait = |terminal: &Terminal, check: &dyn Fn() -> bool| {
                let deadline = Instant::now() + Duration::from_secs(3);
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
            app.runtime.edit(ctx, root, None, None);
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
            app.runtime.edit(ctx, root, None, None);
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
