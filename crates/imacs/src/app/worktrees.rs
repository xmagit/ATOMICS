use super::*;

impl State {
    /// Checks out `branch` in a new worktree, from `start` (a plan's integration branch)
    /// or base_ref, and starts `launch` in it.
    pub(super) fn create_worktree(&mut self, repo: PathBuf, branch: String, start: Option<String>, launch: Launch) {
        let path = git::worktree_path(&self.config.worktree_path, &repo, &branch);
        let config = self.project_config(&repo);
        let (tx, base_ref) = (self.worktree_tx.clone(), config.base_ref);
        let warm = config.worktrees.warm;
        let common = match git::common_dir(&repo) {
            Ok(common) => common,
            Err(err) => {
                let _ = tx.send((branch, Err(format!("{err:#}")), launch, repo));
                return;
            }
        };
        let owners = self.branch_owners(&common, &branch);
        let reuse = match owners.as_slice() {
            [(index, Some(task))] => {
                launch.task.as_ref().is_some_and(|(plan, id)| self.plans[*index].id == *plan && id == task)
            }
            [(_, None)] => launch.task.is_none(),
            _ => false,
        };
        let conflict = !owners.is_empty() && !reuse;
        self.creating.push((common.clone(), branch.clone(), path.clone()));
        self.toast(format!("Creating worktree {}…", paths::display(&path)));
        std::thread::spawn(move || {
            // A retried task finds its worktree from the first attempt.
            let existing = reuse
                && git::common_dir(&path).is_ok_and(|dir| dir == common)
                && git::worktree_of(&path).and_then(|w| w.branch).is_some_and(|b| b == branch);
            let result = if conflict {
                Err(format!("Branch {branch} is already owned by another task or plan in {}", repo.display()))
            } else if existing {
                Ok(PreparedWorktree { path, warmed: false })
            } else {
                git::add_worktree_from(&repo, &path, &branch, start.as_deref(), &base_ref)
                    .map(|()| {
                        let warmed = git::warm_worktree(&repo, &path, &warm);
                        PreparedWorktree { path, warmed }
                    })
                    .map_err(|e| format!("{e:#}"))
            };
            let _ = tx.send((branch, result, launch, common));
        });
    }

    pub(super) fn open_cleanup(&mut self, repo: PathBuf, preselect: Option<PathBuf>) {
        let (tx, rx) = self.wake.channel();
        let mut cleanup =
            Cleanup { repo, rows: None, sandboxes: Default::default(), busy: None, log: Vec::new(), tx, rx };
        self.load_cleanup(&mut cleanup);
        self.cleanup = Some(cleanup);
        self.client_events.push(ClientEvent::Cleanup { preselect });
    }

    fn load_cleanup(&self, cleanup: &mut Cleanup) {
        cleanup.rows = None;
        cleanup.sandboxes = Default::default();
        let (tx, repo, base) = (cleanup.tx.clone(), cleanup.repo.clone(), self.project_config(&cleanup.repo).base_ref);
        let sbx = resolve_program(&self.sbx);
        std::thread::spawn(move || {
            let result = git::worktree_health(&repo, &base).map_err(|e| format!("{e:#}"));
            let _ = tx.send(CleanupEvent::Loaded(result));
            // Without sbx there can be no sandboxes, and nothing to say about it.
            if crate::sandbox::installed(&sbx) {
                let sandboxes = crate::sandbox::for_repo(&sbx, &repo).map_err(|e| format!("listing sandboxes: {e:#}"));
                let _ = tx.send(CleanupEvent::Sandboxes(sandboxes));
            }
        });
    }

    /// Runs a git maintenance command for the dialog in the background.
    fn cleanup_git(&self, cleanup: &mut Cleanup, label: &str, args: &'static [&'static str]) {
        cleanup.busy = Some(label.to_string());
        let (tx, repo, label) = (cleanup.tx.clone(), cleanup.repo.clone(), label.to_string());
        std::thread::spawn(move || {
            let message = match git::git(&repo, args) {
                Ok(_) => format!("{label}: done"),
                Err(err) => format!("{label}: {err:#}"),
            };
            let _ = tx.send(CleanupEvent::Finished(message));
        });
    }

    pub(super) fn poll_cleanup(&mut self) -> bool {
        let Some(mut cleanup) = self.cleanup.take() else { return false };
        let mut changed = false;
        let mut reload = false;
        while let Ok(event) = cleanup.rx.try_recv() {
            changed = true;
            match event {
                CleanupEvent::Loaded(Ok(list)) => {
                    cleanup.rows = Some(list);
                }
                CleanupEvent::Loaded(Err(err)) => {
                    cleanup.log.push(err);
                    cleanup.rows = Some(Vec::new());
                }
                CleanupEvent::Sandboxes(Ok(sandboxes)) => cleanup.sandboxes = sandboxes,
                CleanupEvent::Sandboxes(Err(err)) => cleanup.log.push(err),
                CleanupEvent::SandboxRemoved(name, result) => match result {
                    Ok(()) => {
                        cleanup.log.push(format!("removed sandbox {name}"));
                        cleanup.sandboxes.attached.retain(|(_, attached)| *attached != name);
                        cleanup.sandboxes.orphans.retain(|orphan| orphan.name != name);
                    }
                    Err(err) => {
                        cleanup.log.push(format!("kept sandbox {name}: {}", err.lines().last().unwrap_or(&err)))
                    }
                },
                CleanupEvent::Removed(path, result) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    match result {
                        Ok(()) => {
                            cleanup.log.push(format!("removed {name}"));
                            if let Some(rows) = &mut cleanup.rows {
                                rows.retain(|r| r.path != path);
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

        self.cleanup = Some(cleanup);
        changed
    }
}

impl Desktop<'_> {
    pub(super) fn cleanup_dialog(&mut self, ctx: &egui::Context) {
        self.apply_client_events(ctx);
        if !self.client.cleanup_open {
            return;
        }
        let Some(mut cleanup) = self.runtime.cleanup.take() else { return };
        let p = self.runtime.palette.clone();
        let agent_roots: HashSet<PathBuf> = self.runtime.sessions.iter().map(Session::root).collect();
        let live_sandboxes: HashSet<String> = self.runtime.sessions.iter().filter_map(|s| s.sandbox.clone()).collect();
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
                        for h in rows.iter() {
                            let agent = agent_roots.contains(&h.path);
                            let blocked = agent || h.dirty_files > 0 || h.missing;
                            if blocked {
                                self.client.cleanup_selected.remove(&h.path);
                            }
                            let why = if agent {
                                "an agent is working here"
                            } else if h.missing {
                                "folder is gone: use prune missing"
                            } else {
                                "has uncommitted changes"
                            };
                            let mut selected = self.client.cleanup_selected.contains(&h.path);
                            if ui
                                .add_enabled(!blocked, egui::Checkbox::without_text(&mut selected))
                                .on_disabled_hover_text(why)
                                .changed()
                            {
                                if selected {
                                    self.client.cleanup_selected.insert(h.path.clone());
                                } else {
                                    self.client.cleanup_selected.remove(&h.path);
                                }
                            }
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
                                if cleanup.sandboxes.attached.iter().any(|(path, _)| *path == h.path) {
                                    chip(ui, "sandbox".into(), p.muted);
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
                if !cleanup.sandboxes.orphans.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("Sandboxes whose worktree is gone").color(p.foreground));
                    for orphan in &cleanup.sandboxes.orphans {
                        let live = live_sandboxes.contains(&orphan.name);
                        if live {
                            self.client.cleanup_sandboxes.remove(&orphan.name);
                        }
                        let mut selected = self.client.cleanup_sandboxes.contains(&orphan.name);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!live, egui::Checkbox::without_text(&mut selected))
                                .on_disabled_hover_text("an agent is running in it")
                                .changed()
                            {
                                if selected {
                                    self.client.cleanup_sandboxes.insert(orphan.name.clone());
                                } else {
                                    self.client.cleanup_sandboxes.remove(&orphan.name);
                                }
                            }
                            ui.label(RichText::new(&orphan.name).color(p.foreground));
                            let workspace = orphan.workspace().map(paths::display).unwrap_or_default();
                            ui.label(
                                RichText::new(format!("{} · {workspace}", orphan.status)).color(p.muted).size(11.0),
                            );
                        });
                    }
                }
                ui.separator();
                let selected = rows.iter().filter(|r| self.client.cleanup_selected.contains(&r.path)).count();
                let orphans = cleanup
                    .sandboxes
                    .orphans
                    .iter()
                    .filter(|orphan| self.client.cleanup_sandboxes.contains(&orphan.name))
                    .count();
                ui.horizontal(|ui| {
                    if ui
                        .button("select safe")
                        .on_hover_text("Clean, and merged or with its remote branch deleted")
                        .clicked()
                    {
                        self.client.cleanup_selected = rows
                            .iter()
                            .filter(|h| h.is_safe_to_remove() && !agent_roots.contains(&h.path))
                            .map(|h| h.path.clone())
                            .collect();
                    }
                    if ui.button("select none").clicked() {
                        self.client.cleanup_selected.clear();
                        self.client.cleanup_sandboxes.clear();
                    }
                    ui.checkbox(
                        &mut self.client.cleanup_delete_branches,
                        "also delete their branches (only if merged)",
                    );
                    let mut label = format!("Remove {selected} worktree{}", if selected == 1 { "" } else { "s" });
                    if orphans > 0 {
                        label.push_str(&format!(" and {orphans} sandbox{}", if orphans == 1 { "" } else { "es" }));
                    }
                    let any = selected + orphans > 0;
                    let (text, fill) = if any { (p.on_accent, p.error) } else { (p.muted, p.raised) };
                    let button = egui::Button::new(RichText::new(label).color(text)).fill(fill);
                    if ui.add_enabled(any && cleanup.busy.is_none(), button).clicked() {
                        remove = true;
                    }
                });
                for line in cleanup.log.iter().rev().take(6) {
                    ui.label(RichText::new(line).color(p.muted).size(11.0));
                }
            });

        if let Some(action) = action {
            self.post(move |state| {
                if let Some(mut cleanup) = state.cleanup.take() {
                    match action {
                        "fetch" => state.cleanup_git(&mut cleanup, "fetch --prune", &["fetch", "--prune", "--quiet"]),
                        "prune" => state.cleanup_git(&mut cleanup, "prune", &["worktree", "prune"]),
                        "reload" => state.load_cleanup(&mut cleanup),
                        _ => {}
                    }
                    state.cleanup = Some(cleanup);
                }
            });
        }
        if remove && let Some(rows) = &cleanup.rows {
            let sandboxes = &cleanup.sandboxes;
            let targets: Vec<(PathBuf, Option<String>, Vec<String>)> = rows
                .iter()
                .filter(|r| self.client.cleanup_selected.contains(&r.path))
                .map(|r| {
                    let names = sandboxes.attached.iter().filter(|(path, _)| *path == r.path).map(|(_, n)| n.clone());
                    (r.path.clone(), r.branch.clone(), names.collect())
                })
                .collect();
            let orphans: Vec<String> = sandboxes
                .orphans
                .iter()
                .filter(|o| self.client.cleanup_sandboxes.contains(&o.name) && !live_sandboxes.contains(&o.name))
                .map(|o| o.name.clone())
                .collect();
            cleanup.busy = Some(format!("removing {} worktrees…", targets.len()));
            let (tx, repo, delete) = (cleanup.tx.clone(), cleanup.repo.clone(), self.client.cleanup_delete_branches);
            let sbx = resolve_program(&self.runtime.sbx);
            self.post(move |_| {
                std::thread::spawn(move || {
                    let remove_sandbox = |name: String| {
                        let result = crate::sandbox::remove(&sbx, &name).map_err(|e| format!("{e:#}"));
                        let _ = tx.send(CleanupEvent::SandboxRemoved(name, result));
                    };
                    for (path, branch, sandboxes) in targets {
                        let result =
                            git::remove_worktree(&repo, &path, branch.as_deref(), delete).map_err(|e| format!("{e:#}"));
                        let removed = result.is_ok();
                        let _ = tx.send(CleanupEvent::Removed(path, result));
                        // A worktree's sandbox goes with it; a kept worktree keeps its sandbox.
                        if removed {
                            sandboxes.into_iter().for_each(remove_sandbox);
                        }
                    }
                    orphans.into_iter().for_each(remove_sandbox);
                    let _ = tx.send(CleanupEvent::Finished("removal finished".into()));
                });
            });
        }
        self.client.cleanup_open = open;
        self.runtime.cleanup = Some(cleanup);
        if !open {
            self.runtime.worktrees_at = None;
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::super::tests::{repository, with_state};
    use super::*;

    #[test]
    fn cache_warming_finishes_on_the_worktree_job_before_agent_launch() {
        with_state(
            "app::worktrees::tests::cache_warming_finishes_on_the_worktree_job_before_agent_launch",
            |mut state, ctx| {
                let (_dir, repo) = repository();
                state.config.base_ref = "main".into();
                state.config.agents.push(imacs_core::config::AgentConfig {
                    name: "probe".into(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "if test -f target/dependency; then cat target/dependency; else printf cold; fi > launched.tmp; mv launched.tmp launched"
                            .into(),
                    ],
                    kind: AgentKind::Shell,
                    busy_text: None,
                    isolation: None,
                });
                let supported = git::probe_cache_clone(&repo, repo.parent().unwrap()).unwrap();
                for (branch, folders, expected) in [
                    ("missing", vec!["target".into()], "cold"),
                    ("warm", vec!["target".into()], if supported { "main cache" } else { "cold" }),
                    ("off", vec![], "cold"),
                    ("failed", vec!["target".into()], "cold"),
                ] {
                    if branch == "warm" {
                        std::fs::create_dir(repo.join("target")).unwrap();
                        std::fs::write(repo.join("target/dependency"), "main cache").unwrap();
                    }
                    if branch == "failed" {
                        // A socket cannot be reflinked, even when QA runs as root.
                        let _ = std::fs::remove_file(repo.join("target/dependency"));
                        let socket = std::os::unix::net::UnixListener::bind(repo.join("target/dependency")).unwrap();
                        drop(socket);
                    }
                    state.config.worktrees.warm = folders;
                    let started = Instant::now();
                    state.create_worktree(
                        repo.clone(),
                        branch.into(),
                        None,
                        Launch { agent: "probe".into(), prompt: None, args: vec![], reply: None, task: None },
                    );
                    let event = state.worktree_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    let prepared = event.1.as_ref().unwrap();
                    let path = prepared.path.clone();
                    assert_eq!(prepared.warmed, branch == "warm" && supported);
                    assert!(!path.join("launched").exists());
                    if branch == "failed" {
                        assert!(started.elapsed() < Duration::from_secs(2));
                        assert!(!path.join("target").exists());
                    }
                    state.worktree_tx.send(event).unwrap();
                    state.runtime_tick(ctx);
                    if branch == "warm" && supported {
                        assert_eq!(state.toast.as_ref().unwrap().0, "warm cache from main");
                    }
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !path.join("launched").exists() {
                        assert!(Instant::now() < deadline, "agent never launched for {branch}");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert_eq!(std::fs::read_to_string(path.join("launched")).unwrap(), expected);
                }
            },
        );
    }
}
