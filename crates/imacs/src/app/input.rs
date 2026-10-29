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

    pub(super) fn open_new_dialog(&mut self, folder: Option<PathBuf>) {
        let folder = folder.or_else(|| self.runtime.diff_target()).unwrap_or_else(|| self.runtime.launch_dir.clone());
        let repo = self.runtime.repos.first().cloned().unwrap_or_else(|| folder.clone());
        self.client.new_dialog = Some(NewDialog {
            agent: self.runtime.config.agents.first().map(|a| a.name.clone()).unwrap_or_default(),
            new_worktree: false,
            folder: paths::display(&folder),
            repo: paths::display(&repo),
            branch: String::new(),
            error: None,
        });
    }

    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        self.quick_open_dialog(ctx);
        self.cleanup_dialog(ctx);
        if let Some((id, status)) = self.client.confirm_plan_close.clone() {
            if let Some(index) = self.runtime.plans.iter().position(|p| {
                p.id == id && (p.is_open() || (status == PlanStatus::Archived && p.status == PlanStatus::Completed))
            }) {
                let archive = status == PlanStatus::Archived;
                let mut open = true;
                egui::Window::new(if archive { "Archive plan?" } else { "Complete plan?" })
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                    .open(&mut open)
                    .show(ctx, |ui| {
                        ui.label(format!(
                            "Close “{}”? You can reopen it from Closed plans.",
                            self.runtime.plans[index].goal
                        ));
                        ui.horizontal(|ui| {
                            if ui.button(if archive { "Confirm archive" } else { "Confirm complete" }).clicked() {
                                self.post_plan(index, move |state, index| {
                                    if let Err(err) = state.close_plan(index, status) {
                                        state.toast(err);
                                    }
                                });
                                self.client.confirm_plan_close = None;
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
            let title = self.runtime.sessions.iter().find(|s| s.id == id).map(|s| s.title.clone());
            let Some(title) = title else {
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
                    ui.label(format!("Closing “{title}” stops its process. Claude sessions can be resumed later."));
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

        let Some(mut dialog) = self.client.new_dialog.take() else { return };
        let mut keep = true;
        let mut start: Option<(String, PathBuf)> = None;
        let mut create: Option<(PathBuf, String, Launch)> = None;
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
                        ui.radio_value(&mut dialog.new_worktree, false, "Existing folder");
                        ui.radio_value(&mut dialog.new_worktree, true, "New worktree");
                    });
                    ui.end_row();
                    if dialog.new_worktree {
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
                            ui.text_edit_singleline(&mut dialog.folder);
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
                });
                if let Some(err) = &dialog.error {
                    ui.label(RichText::new(err).color(self.runtime.palette.error));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let enter = ui.input(|i| i.key_pressed(Key::Enter));
                    if ui.button("Start").clicked() || enter {
                        if dialog.new_worktree {
                            let repo = paths::expand(dialog.repo.trim());
                            let branch = dialog.branch.trim().to_string();
                            let launch = Launch {
                                agent: dialog.agent.clone(),
                                prompt: None,
                                args: Vec::new(),
                                reply: None,
                                task: None,
                            };
                            create = Some((repo, branch, launch));
                            keep = false;
                        } else {
                            start = Some((dialog.agent.clone(), paths::expand(dialog.folder.trim())));
                        }
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
            });
        if let Some((repo, branch, launch)) = create {
            self.create_worktree(repo, branch, None, launch);
        }
        if let Some(target) = start {
            self.post(move |state| state.pending_open.push(target));
            keep = false;
        }
        if keep {
            self.client.new_dialog = Some(dialog);
        }
    }
}
