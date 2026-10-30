use super::*;

impl State {
    // ── input ─────────────────────────────────────────────────────────────
}

impl Desktop<'_> {
    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        let keys = self.runtime.config.keys.clone();
        let pressed = |text: &str| shortcut(text).is_some_and(|s| ctx.input_mut(|i| i.consume_shortcut(&s)));
        if pressed(&keys.voice) {
            self.toggle_voice();
        }
        if matches!(self.client.voice, Voice::Opening(_) | Voice::Recording(_))
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
        {
            if let Voice::Opening(cancelled) = &self.client.voice {
                cancelled.store(true, Ordering::Relaxed);
            }
            self.client.voice = Voice::Idle;
            self.runtime.toast("Voice: recording cancelled");
        }
        if pressed(&keys.toggle_diff) {
            self.runtime.diff_open = !self.runtime.diff_open;
        }
        if pressed(&keys.focus_steer) {
            self.client.focus_steer = true;
        }
        if pressed(&keys.toggle_editor)
            && let Some(root) = self.runtime.diff_target()
        {
            if self.runtime.panel == PanelMode::Editor {
                self.runtime.panel = PanelMode::Diff;
            } else {
                let state = self.client.diff_views.get(&root);
                let file = state.and_then(|s| s.viewing.clone()).or_else(|| {
                    state
                        .and_then(|s| s.prepared.as_ref())
                        .and_then(|p| p.diff.files.first().map(|f| f.path().to_string()))
                });
                self.edit(ctx, &root, file.as_deref(), None);
            }
        }
        // Shift+↓/↑ walk the changed files from anywhere but a text field or the editor.
        let editor_focused = self.client.editor_id.is_some_and(|id| ctx.memory(|m| m.has_focus(id)));
        if !ctx.text_edit_focused() && !editor_focused {
            let step = ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::SHIFT, Key::ArrowDown) {
                    1
                } else if i.consume_key(egui::Modifiers::SHIFT, Key::ArrowUp) {
                    -1
                } else {
                    0
                }
            });
            if step != 0
                && let Some(root) = self.runtime.diff_target()
            {
                self.runtime.diff_open = true;
                if self.runtime.panel == PanelMode::Plan {
                    self.runtime.panel = PanelMode::Diff;
                }
                let moved = self.client.diff_views.entry(root.clone()).or_default().step(step);
                if self.runtime.panel != PanelMode::Editor
                    && let Some((path, _)) = &moved
                {
                    self.open_file(&root, path);
                }
                if self.runtime.panel == PanelMode::Editor
                    && let Some((path, line)) = moved
                {
                    self.edit(ctx, &root, Some(&path), line);
                    self.client.focus_editor = false;
                }
            }
        }
        if pressed(&keys.toggle_plan) {
            self.runtime.toggle_plan();
        }
        if pressed(&keys.toggle_chief) {
            self.toggle_chief(ctx);
        }
        if pressed(&keys.quick_open) && self.runtime.diff_target().is_some() {
            self.client.quick_open = Some(QuickOpen { query: String::new(), selected: 0, focus: true });
        }
        if pressed(&keys.new_session) {
            self.open_new_dialog(None);
        }
        if pressed(&keys.refresh_diff)
            && let Some(root) = self.runtime.diff_target()
        {
            self.post(move |runtime| state_refresh(runtime.diffs.entry(root).or_default()));
        }
        if pressed(&keys.next_attention) {
            let start = self.runtime.active_index().map(|i| i + 1).unwrap_or(0);
            let n = self.runtime.sessions.len();
            if let Some(i) =
                (0..n).map(|k| (start + k) % n).find(|&i| self.runtime.sessions[i].status.needs_attention())
            {
                let id = self.runtime.sessions[i].id.clone();
                self.activate(&id, true);
            } else {
                self.runtime.toast("Nothing needs you right now");
            }
        }
        if pressed(&keys.close_session)
            && let Some(id) = self.runtime.active.clone()
        {
            self.client.confirm_close = Some(id);
        }
        for (n, key) in
            [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9]
                .into_iter()
                .enumerate()
        {
            let chord = egui::KeyboardShortcut::new(shortcut("cmd+a").map(|s| s.modifiers).unwrap_or_default(), key);
            // cmd+N jumps to the Nth agent on screen and loads its diff.
            let visible = self.runtime.visible_sessions();
            if ctx.input_mut(|i| i.consume_shortcut(&chord))
                && let Some(id) = visible.get(n).map(|&i| self.runtime.sessions[i].id.clone())
            {
                self.activate(&id, true);
                if self.runtime.panel == PanelMode::Plan {
                    self.runtime.panel = PanelMode::Diff;
                }
            }
        }
    }

    /// Opens the New session dialog. A new session works in a scratch folder of its own unless
    /// `folder` or the user picks one; it never takes the focused tile's folder by itself.
    pub(super) fn open_new_dialog(&mut self, folder: Option<PathBuf>) {
        let repo = self.runtime.repos.first().cloned().or_else(|| folder.clone()).unwrap_or_default();
        self.client.new_dialog = Some(NewDialog {
            agent: self.runtime.config.agents.first().map(|a| a.name.clone()).unwrap_or_default(),
            place: if folder.is_some() { Place::Folder } else { Place::Scratch },
            folder: folder.as_deref().map(paths::display).unwrap_or_default(),
            repo: paths::display(&repo),
            branch: String::new(),
            prompt: String::new(),
            isolation: None,
            error: None,
        });
    }

    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        self.quick_open_dialog(ctx);
        self.cleanup_dialog(ctx);
        self.config_proposal_dialog(ctx);
        if let Some((id, _)) = self.client.confirm_plan_close.clone() {
            if let Some(index) =
                self.runtime.plans.iter().position(|plan| plan.id == id && plan.status == PlanStatus::Open)
            {
                let mut open = true;
                egui::Window::new("Close plan?")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(format!(
                            "Close “{}”? You can view or reopen it from History.",
                            self.runtime.plans[index].goal
                        ));
                        ui.horizontal(|ui| {
                            for (label, status) in
                                [("Complete plan", PlanStatus::Completed), ("Archive plan", PlanStatus::Archived)]
                            {
                                if ui.button(label).clicked() {
                                    self.post_plan(index, move |state, index| {
                                        if let Err(error) = state.close_plan(index, status) {
                                            state.toast(error);
                                        }
                                    });
                                    self.client.confirm_plan_close = None;
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                self.client.confirm_plan_close = None;
                            }
                        });
                    });
                if !open {
                    self.client.confirm_plan_close = None;
                }
            } else {
                self.client.confirm_plan_close = None;
            }
        }
        self.plan_cleanup_dialog(ctx);
        if let Some(id) = self.client.confirm_close.clone() {
            let session = self.runtime.sessions.iter().find(|s| s.id == id);
            let Some((title, scratch)) = session.map(|s| (s.title.clone(), s.scratch.clone())) else {
                self.client.confirm_close = None;
                return;
            };
            let mut open = true;
            egui::Window::new("Close session?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(match &scratch {
                        Some(folder) => format!(
                            "Closing “{title}” stops its process and deletes its scratch folder {}.",
                            paths::display(folder)
                        ),
                        None => format!("Closing “{title}” stops its process. Claude sessions can be resumed later."),
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Close session").clicked() {
                            self.close(&id);
                            self.client.confirm_close = None;
                        }
                        if ui.button("Cancel").clicked() {
                            self.client.confirm_close = None;
                        }
                    });
                });
            if !open {
                self.client.confirm_close = None;
            }
        }

        self.rename_dialog(ctx);
        let Some(mut dialog) = self.client.new_dialog.take() else { return };
        let mut keep = true;
        let mut start: Option<ControlRequest> = None;
        egui::Window::new("New session")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                egui::Grid::new("new-session").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Agent");
                    egui::ComboBox::from_id_salt("agent").selected_text(&dialog.agent).show_ui(ui, |ui| {
                        for agent in &self.runtime.config.agents {
                            ui.selectable_value(&mut dialog.agent, agent.name.clone(), &agent.name);
                        }
                    });
                    ui.end_row();
                    ui.label("Where");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut dialog.place, Place::Scratch, "Scratch folder").on_hover_text(
                            "A new folder of its own, outside any project, removed when the session closes",
                        );
                        ui.radio_value(&mut dialog.place, Place::Folder, "Existing folder");
                        ui.radio_value(&mut dialog.place, Place::Worktree, "New worktree");
                    });
                    ui.end_row();
                    if dialog.place == Place::Scratch {
                        ui.label("");
                        ui.label(
                            RichText::new(format!("in {}", paths::display(&paths::scratch_dir().join("…"))))
                                .color(self.runtime.palette.muted)
                                .size(11.5),
                        );
                        ui.end_row();
                    } else if dialog.place == Place::Worktree {
                        ui.label("Repository");
                        ui.text_edit_singleline(&mut dialog.repo);
                        ui.end_row();
                        ui.label("Branch");
                        ui.text_edit_singleline(&mut dialog.branch).request_focus();
                        ui.end_row();
                        if !dialog.branch.trim().is_empty() {
                            let path = git::worktree_path(
                                &self.runtime.config.worktree_path,
                                &paths::expand(dialog.repo.trim()),
                                dialog.branch.trim(),
                            );
                            ui.label("");
                            ui.label(RichText::new(paths::display(&path)).monospace().size(11.5));
                            ui.end_row();
                        }
                    } else {
                        ui.label("Folder");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut dialog.folder).hint_text("~/repos/app"));
                            ui.menu_button("▾", |ui| {
                                let mut known: Vec<PathBuf> =
                                    self.runtime.worktrees.iter().map(|w| w.path.clone()).collect();
                                known.extend(self.runtime.sessions.iter().map(Session::root));
                                known.sort();
                                known.dedup();
                                for path in known {
                                    if ui.button(paths::display(&path)).clicked() {
                                        dialog.folder = paths::display(&path);
                                        ui.close();
                                    }
                                }
                            });
                        });
                        ui.end_row();
                    }
                    if let Some(agent) = self.runtime.config.agent(&dialog.agent) {
                        // What the settings give this agent; a repository that requires sandboxes still wins.
                        let configured = self.runtime.effective_isolation().mode_for(agent, None);
                        let name = |mode: Option<imacs_core::isolation::IsolationMode>| match mode {
                            Some(imacs_core::isolation::IsolationMode::Sandbox) => "Sandboxed",
                            Some(imacs_core::isolation::IsolationMode::Bare) => "Bare, on this machine",
                            None => "As configured",
                        };
                        ui.label("Isolation");
                        let selected = match dialog.isolation {
                            None => format!("As configured · {}", configured.name()),
                            mode => name(mode).to_string(),
                        };
                        egui::ComboBox::from_id_salt("isolation").selected_text(selected).show_ui(ui, |ui| {
                            for mode in [
                                None,
                                Some(imacs_core::isolation::IsolationMode::Sandbox),
                                Some(imacs_core::isolation::IsolationMode::Bare),
                            ] {
                                ui.selectable_value(&mut dialog.isolation, mode, name(mode));
                            }
                        });
                        ui.end_row();
                    }
                    ui.label("Prompt");
                    let prompt = ui.add(
                        egui::TextEdit::singleline(&mut dialog.prompt)
                            .hint_text("optional first prompt; it names a scratch session")
                            .desired_width(320.0),
                    );
                    if dialog.place == Place::Scratch {
                        prompt.request_focus();
                    }
                    ui.end_row();
                });
                if let Some(err) = &dialog.error {
                    ui.label(RichText::new(err).color(self.runtime.palette.error));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let enter = ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Start").clicked() || enter {
                        match new_session_request(&dialog) {
                            Ok(request) => start = Some(request),
                            Err(error) => dialog.error = Some(error),
                        }
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
            });
        if let Some(request) = start {
            // A new session is outside any plan, so it lands in the Ungrouped tab.
            self.runtime.show_ungrouped();
            self.runtime.client_commands.push(request);
            keep = false;
        }
        if keep {
            self.client.new_dialog = Some(dialog);
        }
    }

    fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut rename) = self.client.rename.take() else { return };
        if !self.runtime.sessions.iter().any(|s| s.id == rename.session) {
            return;
        }
        let mut keep = true;
        egui::Window::new("Rename session")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let field = ui.add(egui::TextEdit::singleline(&mut rename.title).desired_width(280.0));
                if std::mem::take(&mut rename.focus) {
                    field.request_focus();
                }
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        let (id, title) = (rename.session.clone(), rename.title.clone());
                        self.post(move |state| {
                            if let Some(session) = state.sessions.iter_mut().find(|s| s.id == id) {
                                session.rename(&title);
                            }
                            state.remember(&id);
                        });
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
            });
        if keep {
            self.client.rename = Some(rename);
        }
    }

    /// Opens the rename dialog for a session.
    pub(super) fn begin_rename(&mut self, id: &str) {
        if let Some(session) = self.runtime.sessions.iter().find(|s| s.id == id && !s.chief) {
            self.client.rename = Some(Rename { session: id.into(), title: session.title.clone(), focus: true });
        }
    }
}

/// What the New session dialog starts: a scratch session, or one in a folder or new worktree.
pub(super) fn new_session_request(dialog: &NewDialog) -> Result<ControlRequest, String> {
    let text = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_string());
    let (folder, repo, branch) = match dialog.place {
        Place::Scratch => (None, None, None),
        Place::Folder => {
            let folder = text(&dialog.folder).ok_or("Pick a folder")?;
            (Some(paths::expand(&folder).to_string_lossy().into_owned()), None, None)
        }
        Place::Worktree => {
            let repo = text(&dialog.repo).ok_or("Pick a repository")?;
            let branch = text(&dialog.branch).ok_or("Name the new branch")?;
            (None, Some(paths::expand(&repo).to_string_lossy().into_owned()), Some(branch))
        }
    };
    Ok(ControlRequest::Start {
        agent: dialog.agent.clone(),
        folder,
        repo,
        branch,
        prompt: text(&dialog.prompt),
        args: Vec::new(),
        isolation: dialog.isolation,
    })
}
