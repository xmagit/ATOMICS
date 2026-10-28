use super::*;

impl App {
    /// Checks out `branch` in a new worktree, from `start` (a plan's integration branch)
    /// or base_ref, and starts `launch` in it.
    pub(super) fn create_worktree(&mut self, repo: PathBuf, branch: String, start: Option<String>, launch: Launch) {
        let path = git::worktree_path(&self.config.worktree_path, &repo, &branch);
        let (tx, base_ref, ctx) = (self.worktree_tx.clone(), self.config.base_ref.clone(), self.ctx.clone());
        self.creating.push(branch.clone());
        self.toast(format!("Creating worktree {}…", paths::display(&path)));
        std::thread::spawn(move || {
            // A retried task finds its worktree from the first attempt.
            let existing = git::worktree_of(&path).and_then(|w| w.branch).is_some_and(|b| b == branch);
            let result = if existing {
                Ok(path)
            } else {
                git::add_worktree_from(&repo, &path, &branch, start.as_deref(), &base_ref)
                    .map(|()| path)
                    .map_err(|e| format!("{e:#}"))
            };
            let _ = tx.send((branch, result, launch));
            ctx.request_repaint();
        });
    }

    pub(super) fn open_cleanup(&mut self, repo: PathBuf, preselect: Option<PathBuf>) {
        let (tx, rx) = channel();
        let mut cleanup =
            Cleanup { repo, rows: None, preselect, delete_branches: true, busy: None, log: Vec::new(), tx, rx };
        self.load_cleanup(&mut cleanup);
        self.cleanup = Some(cleanup);
    }

    fn load_cleanup(&self, cleanup: &mut Cleanup) {
        cleanup.rows = None;
        let (tx, repo, base, ctx) =
            (cleanup.tx.clone(), cleanup.repo.clone(), self.config.base_ref.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let result = git::worktree_health(&repo, &base).map_err(|e| format!("{e:#}"));
            let _ = tx.send(CleanupEvent::Loaded(result));
            ctx.request_repaint();
        });
    }

    /// Runs a git maintenance command for the dialog in the background.
    fn cleanup_git(&self, cleanup: &mut Cleanup, label: &str, args: &'static [&'static str]) {
        cleanup.busy = Some(label.to_string());
        let (tx, repo, ctx, label) = (cleanup.tx.clone(), cleanup.repo.clone(), self.ctx.clone(), label.to_string());
        std::thread::spawn(move || {
            let message = match git::git(&repo, args) {
                Ok(_) => format!("{label}: done"),
                Err(err) => format!("{label}: {err:#}"),
            };
            let _ = tx.send(CleanupEvent::Finished(message));
            ctx.request_repaint();
        });
    }

    pub(super) fn cleanup_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut cleanup) = self.cleanup.take() else { return };
        let mut reload = false;
        while let Ok(event) = cleanup.rx.try_recv() {
            match event {
                CleanupEvent::Loaded(Ok(list)) => {
                    let preselect = cleanup.preselect.take();
                    cleanup.rows = Some(
                        list.into_iter()
                            .map(|health| CleanupRow { selected: preselect.as_ref() == Some(&health.path), health })
                            .collect(),
                    );
                }
                CleanupEvent::Loaded(Err(err)) => {
                    cleanup.log.push(err);
                    cleanup.rows = Some(Vec::new());
                }
                CleanupEvent::Removed(path, result) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            cleanup.log.push(format!("removed {name}"));
                            if let Some(rows) = &mut cleanup.rows {
                                rows.retain(|r| r.health.path != path);
                            }
                        }
                        Err(err) => cleanup.log.push(format!("kept {name}: {}", err.lines().last().unwrap_or(&err))),
                    }
                }
                CleanupEvent::Finished(message) => {
                    reload = !message.starts_with("removal");
                    cleanup.log.push(message);
                    cleanup.busy = None;
                    self.worktrees_at = None;
                }
            }
        }
        if reload {
            self.load_cleanup(&mut cleanup);
        }

        let p = self.palette.clone();
        let agent_roots: HashSet<PathBuf> = self.sessions.iter().map(Session::root).collect();
        let name = cleanup.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut open = true;
        let mut remove = false;
        let mut action: Option<&str> = None;
        egui::Window::new(format!("Clean up worktrees · {name}"))
            .open(&mut open)
            .collapsible(false)
            .default_size(vec2(900.0, 620.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let idle = cleanup.busy.is_none();
                    if ui
                        .add_enabled(idle, egui::Button::new("fetch --prune"))
                        .on_hover_text("Refresh which remote branches are gone (completed PRs)")
                        .clicked()
                    {
                        action = Some("fetch");
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("prune missing"))
                        .on_hover_text("Forget worktrees whose folder no longer exists")
                        .clicked()
                    {
                        action = Some("prune");
                    }
                    if ui.add_enabled(idle, egui::Button::new("reload")).clicked() {
                        action = Some("reload");
                    }
                    if let Some(busy) = &cleanup.busy {
                        ui.spinner();
                        ui.label(RichText::new(busy).color(p.muted));
                    }
                });
                ui.separator();
                let Some(rows) = &mut cleanup.rows else {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("Checking every worktree for uncommitted work and merged branches…")
                                .color(p.muted),
                        );
                    });
                    return;
                };
                egui::ScrollArea::vertical().id_salt("cleanup-rows").max_height(440.0).show(ui, |ui| {
                    egui::Grid::new("cleanup-grid").num_columns(5).spacing([14.0, 6.0]).striped(true).show(ui, |ui| {
                        for row in rows.iter_mut() {
                            let h = &row.health;
                            let agent = agent_roots.contains(&h.path);
                            let blocked = agent || h.dirty_files > 0 || h.missing;
                            if blocked {
                                row.selected = false;
                            }
                            let why = if agent {
                                "an agent is working here"
                            } else if h.missing {
                                "folder is gone: use prune missing"
                            } else {
                                "has uncommitted changes"
                            };
                            ui.add_enabled(!blocked, egui::Checkbox::without_text(&mut row.selected))
                                .on_disabled_hover_text(why);
                            let folder =
                                h.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            ui.label(RichText::new(folder).color(p.foreground));
                            let branch = h.branch.clone().unwrap_or_else(|| "detached".into());
                            ui.label(RichText::new(branch).color(p.muted).size(11.5));
                            ui.horizontal(|ui| {
                                let chip = |ui: &mut egui::Ui, text: String, color: Color32| {
                                    ui.label(RichText::new(text).color(color).size(11.0));
                                };
                                if h.missing {
                                    chip(ui, "missing".into(), p.error);
                                }
                                if agent {
                                    chip(ui, "● agent".into(), p.accent);
                                }
                                if h.dirty_files > 0 {
                                    chip(ui, format!("{} uncommitted", h.dirty_files), p.warning);
                                }
                                if h.merged {
                                    chip(ui, "merged".into(), p.success);
                                }
                                if h.upstream_gone {
                                    chip(ui, "remote gone".into(), p.success);
                                }
                                if !h.missing && h.dirty_files == 0 && !h.merged && !h.upstream_gone {
                                    chip(ui, "unmerged".into(), p.muted);
                                }
                            });
                            ui.label(
                                RichText::new(h.last_commit.clone().unwrap_or_default()).color(p.muted).size(11.0),
                            );
                            ui.end_row();
                        }
                    });
                });
                ui.separator();
                let selected = rows.iter().filter(|r| r.selected).count();
                ui.horizontal(|ui| {
                    if ui
                        .button("select safe")
                        .on_hover_text("Clean, and merged or with its remote branch deleted")
                        .clicked()
                    {
                        for row in rows.iter_mut() {
                            row.selected = row.health.is_safe_to_remove() && !agent_roots.contains(&row.health.path);
                        }
                    }
                    if ui.button("select none").clicked() {
                        for row in rows.iter_mut() {
                            row.selected = false;
                        }
                    }
                    ui.checkbox(&mut cleanup.delete_branches, "also delete their branches (only if merged)");
                    let label = format!("Remove {selected} worktree{}", if selected == 1 { "" } else { "s" });
                    let button = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(if selected > 0 {
                        p.error.gamma_multiply(0.55)
                    } else {
                        p.raised
                    });
                    if ui.add_enabled(selected > 0 && cleanup.busy.is_none(), button).clicked() {
                        remove = true;
                    }
                });
                for line in cleanup.log.iter().rev().take(6) {
                    ui.label(RichText::new(line).color(p.muted).size(11.0));
                }
            });

        match action {
            Some("fetch") => self.cleanup_git(&mut cleanup, "fetch --prune", &["fetch", "--prune", "--quiet"]),
            Some("prune") => self.cleanup_git(&mut cleanup, "prune", &["worktree", "prune"]),
            Some("reload") => self.load_cleanup(&mut cleanup),
            _ => {}
        }
        if remove && let Some(rows) = &cleanup.rows {
            let targets: Vec<(PathBuf, Option<String>)> =
                rows.iter().filter(|r| r.selected).map(|r| (r.health.path.clone(), r.health.branch.clone())).collect();
            cleanup.busy = Some(format!("removing {} worktrees…", targets.len()));
            let (tx, repo, delete, ctx) =
                (cleanup.tx.clone(), cleanup.repo.clone(), cleanup.delete_branches, self.ctx.clone());
            std::thread::spawn(move || {
                for (path, branch) in targets {
                    let result =
                        git::remove_worktree(&repo, &path, branch.as_deref(), delete).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(CleanupEvent::Removed(path, result));
                    ctx.request_repaint();
                }
                let _ = tx.send(CleanupEvent::Finished("removal finished".into()));
                ctx.request_repaint();
            });
        }
        if open {
            self.cleanup = Some(cleanup);
        } else {
            self.worktrees_at = None;
        }
    }
}
