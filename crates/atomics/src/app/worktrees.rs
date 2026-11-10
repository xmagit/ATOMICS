use super::*;

impl State {
    pub(super) fn plan_worktree_path(&self, index: usize, task: Option<&str>, repo: &Path, branch: &str) -> PathBuf {
        let plan = &self.plans[index];
        match task.and_then(|id| plan.tasks.iter().find(|task| task.id == id)) {
            Some(task) => git::plan_task_worktree_path(&self.config.worktree_path, repo, plan, task, branch),
            None => git::worktree_path(&self.config.worktree_path, repo, branch),
        }
    }

    /// Checks out `branch` in a new worktree, from `start` (a plan's integration branch)
    /// or base_ref, and starts `launch` in it.
    pub(super) fn create_worktree(&mut self, repo: PathBuf, branch: String, start: Option<String>, launch: Launch) {
        if let Some(error) = &self.tickets.workspace_identity_error {
            let _ = self.worktree_tx.send((branch, Err(error.clone()), launch, repo));
            return;
        }
        let path = launch
            .task
            .as_ref()
            .and_then(|(plan, task)| self.plans.iter().position(|p| p.id == *plan).map(|index| (index, task)))
            .map(|(index, task)| self.plan_worktree_path(index, Some(task), &repo, &branch))
            .unwrap_or_else(|| git::worktree_path(&self.config.worktree_path, &repo, &branch));
        if let Err(error) = self.check_disk(&path) {
            let _ = self.worktree_tx.send((branch, Err(error), launch, repo));
            return;
        }
        let config = self.project_config(&repo);
        let (tx, base_ref) = (self.worktree_tx.clone(), config.base_ref);
        let warm = config.worktrees.warm;
        let cache = config.cache;
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
        let jobs = self.running_lifecycle();
        std::thread::spawn(move || {
            // A cleanup may still be removing a worktree here, such as a reopened plan's. This thread
            // waits for it, never the UI thread.
            crate::lifecycle::wait_for(&jobs, None, Some(&path));
            // A retried task finds its worktree from the first attempt.
            let existing = reuse
                && git::common_dir(&path).is_ok_and(|dir| dir == common)
                && git::worktree_of(&path).and_then(|w| w.branch).is_some_and(|b| b == branch);
            let result = if conflict {
                Err(format!("Branch {branch} is already owned by another task or plan in {}", repo.display()))
            } else if existing {
                Ok(PreparedWorktree { path, identity: None, warmed: false, start: None, warning: None })
            } else {
                // New work starts from the remote as it is now, not as this machine last fetched it. Only
                // where it starts from base_ref: an integration branch that exists is its own start.
                let fresh = if start.as_deref().is_none_or(|start| !git::branch_exists(&repo, start)) {
                    let _fetch =
                        crate::ref_guard::host_write_refs(&repo, ["refs/remotes/".into(), "refs/tags/".into()]);
                    git::fresh_base(&repo, &base_ref)
                } else {
                    git::FreshBase { base: base_ref.clone(), warning: None }
                };
                if let Some(warning) = &fresh.warning {
                    eprintln!("ATOMICS: {branch} in {}: {warning}", repo.display());
                }
                let _write = crate::ref_guard::host_write(&repo, [&branch]);
                let branch_ref = format!("refs/heads/{branch}");
                // A branch that exists already may hold commits of an earlier attempt: only a new one's start counts.
                let new = git::commit_of(&repo, &branch_ref).is_ok_and(|tip| tip.is_none());
                git::add_worktree_from(&repo, &path, &branch, start.as_deref(), &fresh.base)
                    .map(|()| {
                        if let Err(error) = crate::build_cache::link(&paths::atomics_home(), &path, &cache) {
                            eprintln!("ATOMICS: cache setup: {error:#}");
                        }
                        let warmed = git::warm_worktree(&repo, &path, &warm);
                        let start = new.then(|| git::commit_of(&repo, &branch_ref).ok().flatten()).flatten();
                        PreparedWorktree { path, identity: None, warmed, start, warning: fresh.warning }
                    })
                    .map_err(|e| format!("{e:#}"))
            };
            let _ = tx.send((branch, result, launch, common));
        });
    }

    pub(super) fn open_cleanup(&mut self, repo: PathBuf, preselect: Option<PathBuf>) {
        let (tx, rx) = self.wake.channel();
        let mut cleanup = Cleanup {
            repo,
            rows: None,
            sandboxes: Default::default(),
            busy: None,
            log: crate::lifecycle::leftovers(&paths::atomics_home()),
            tx,
            rx,
        };
        self.load_cleanup(&mut cleanup);
        self.cleanup = Some(cleanup);
        self.client_events.push(ClientEvent::Cleanup { preselect });
    }

    fn load_cleanup(&mut self, cleanup: &mut Cleanup) {
        cleanup.rows = None;
        cleanup.sandboxes = Default::default();
        let (tx, repo, base) = (cleanup.tx.clone(), cleanup.repo.clone(), self.project_config(&cleanup.repo).base_ref);
        let sbx = resolve_program(&self.sbx);
        self.lifecycle(Scope::none(), move || {
            let result = crate::sandbox::load_owned_pins(&paths::atomics_home())
                .and_then(|()| git::worktree_health(&repo, &base))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(CleanupEvent::Loaded(result));
            // Without sbx there can be no sandboxes, and nothing to say about it.
            if crate::sandbox::installed(&sbx) {
                let sandboxes = crate::sandbox::for_repo(&paths::atomics_home(), &sbx, &repo)
                    .map_err(|e| format!("listing sandboxes: {e:#}"));
                let _ = tx.send(CleanupEvent::Sandboxes(sandboxes));
            }
        });
    }

    /// A button of the open cleanup dialog: `fetch` (`git fetch --prune`), `prune` (worktrees whose folder
    /// is gone) or `reload`.
    pub(super) fn cleanup_action(&mut self, action: &str) {
        if let Some(mut cleanup) = self.cleanup.take() {
            match action {
                "fetch" => self.cleanup_git(
                    &mut cleanup,
                    "fetch --prune",
                    &["fetch", "--prune", "--quiet"],
                    &["refs/remotes/", "refs/tags/"],
                ),
                "prune" => self.cleanup_git(&mut cleanup, "prune", &["worktree", "prune"], &[]),
                "reload" => self.load_cleanup(&mut cleanup),
                _ => {}
            }
            self.cleanup = Some(cleanup);
        }
    }

    /// Removes `worktrees` of the open cleanup dialog, each with the sandboxes working there, and the
    /// sandboxes `orphans` whose worktree is gone; with the worktrees' branches if `delete`.
    pub(super) fn remove_worktrees(&mut self, worktrees: &[PathBuf], orphans: Vec<String>, delete: bool) {
        let Some(cleanup) = &self.cleanup else { return };
        let Some(rows) = &cleanup.rows else { return };
        let sandboxes = &cleanup.sandboxes;
        let targets: Vec<(PathBuf, Option<String>, Vec<String>)> = rows
            .iter()
            .filter(|r| worktrees.contains(&r.path))
            .map(|r| {
                let names = sandboxes.attached.iter().filter(|(path, _)| *path == r.path).map(|(_, n)| n.clone());
                (r.path.clone(), r.branch.clone(), names.collect())
            })
            .collect();
        let (tx, repo) = (cleanup.tx.clone(), cleanup.repo.clone());
        let sbx = resolve_program(&self.sbx);
        let scope = Scope {
            sandboxes: targets.iter().flat_map(|(_, _, names)| names.iter().cloned()).chain(orphans.clone()).collect(),
            worktrees: Some(targets.iter().map(|(path, _, _)| path.clone()).collect()),
        };
        self.lifecycle(scope, move || {
            let remove_sandbox = |name: String| {
                let result = crate::sandbox::remove(&paths::atomics_home(), &sbx, &name).map_err(|e| format!("{e:#}"));
                let _ = tx.send(CleanupEvent::SandboxRemoved(name, result));
            };
            for (path, branch, _) in targets {
                let mut names = Vec::new();
                let result = (|| -> anyhow::Result<()> {
                    anyhow::ensure!(
                        git::git(&path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty(),
                        "uncommitted changes"
                    );
                    if crate::sandbox::installed(&sbx) {
                        names = crate::sandbox::in_worktree(&paths::atomics_home(), &sbx, &path)?;
                        for name in &names {
                            crate::sandbox::stop(&paths::atomics_home(), &sbx, name)?;
                        }
                    }
                    let _write = crate::ref_guard::host_write(&repo, branch.iter().filter(|_| delete));
                    git::remove_worktree(&repo, &path, branch.as_deref(), delete)
                })()
                .map_err(|e| format!("{e:#}"));
                let removed = !path.exists();
                let _ = tx.send(CleanupEvent::Removed(path, result));
                // A worktree's sandbox goes with it; a kept worktree keeps its sandbox.
                if removed {
                    names.into_iter().for_each(remove_sandbox);
                }
            }
            orphans.into_iter().for_each(remove_sandbox);
            let _ = tx.send(CleanupEvent::Finished("removal finished".into()));
        });
    }

    /// Runs a git maintenance command for the dialog in the background. It writes `refs`, names or
    /// prefixes ending in `/`, which ref guards then keep as ATOMICS's own.
    fn cleanup_git(&self, cleanup: &mut Cleanup, label: &str, args: &'static [&'static str], refs: &[&str]) {
        cleanup.busy = Some(label.to_string());
        let (tx, repo, label) = (cleanup.tx.clone(), cleanup.repo.clone(), label.to_string());
        let refs: Vec<String> = refs.iter().map(|name| name.to_string()).collect();
        std::thread::spawn(move || {
            let _write = (!refs.is_empty()).then(|| crate::ref_guard::host_write_refs(&repo, refs));
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
        let p = self.palette();
        let agent_roots: HashSet<PathBuf> = self.runtime.sessions.iter().map(Session::root).collect();
        let live_sandboxes: HashSet<String> = self.runtime.sessions.iter().filter_map(|s| s.sandbox.clone()).collect();
        let name = cleanup.repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut open = true;
        let mut remove = false;
        let mut action = None;
        egui::Window::new(format!("Clean up worktrees · {name}"))
            .open(&mut open)
            .collapsible(false)
            .default_size(vec2(900.0, 620.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let idle = cleanup.busy.is_none();
                    if ui
                        .add_enabled(idle, egui::Button::new("Fetch and prune"))
                        .on_hover_text("Refresh which remote branches are gone (completed PRs)")
                        .clicked()
                    {
                        action = Some(Command::CleanupFetch);
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Prune missing"))
                        .on_hover_text("Forget worktrees whose folder no longer exists")
                        .clicked()
                    {
                        action = Some(Command::CleanupPrune);
                    }
                    if ui.add_enabled(idle, egui::Button::new("Reload")).clicked() {
                        action = Some(Command::CleanupReload);
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
                            let blocked = agent || h.dirty_files > 0 || h.missing || h.untrusted;
                            if blocked {
                                self.client.cleanup_selected.remove(&h.path);
                            }
                            let why = if agent {
                                "an agent is working here"
                            } else if h.missing {
                                "folder is gone: use prune missing"
                            } else if h.untrusted {
                                "git lists it from files that do not lead back to it, which a sandbox can write: \
                                 ATOMICS does not run git there"
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
                            ui.label(RichText::new(branch).color(p.muted).size(11.0));
                            ui.horizontal(|ui| {
                                let chip = |ui: &mut egui::Ui, text: String, color: Color32| {
                                    ui.label(RichText::new(text).color(color).size(11.0));
                                };
                                if h.missing {
                                    chip(ui, "missing".into(), p.error);
                                }
                                if h.untrusted {
                                    chip(ui, "untrusted".into(), p.error);
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
                                if !h.missing && !h.untrusted && h.dirty_files == 0 && !h.merged && !h.upstream_gone {
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
                        .button("Select safe")
                        .on_hover_text("Clean, and merged or with its remote branch deleted")
                        .clicked()
                    {
                        self.client.cleanup_selected = rows
                            .iter()
                            .filter(|h| h.is_safe_to_remove() && !agent_roots.contains(&h.path))
                            .map(|h| h.path.clone())
                            .collect();
                    }
                    if ui.button("Select none").clicked() {
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
                egui::ScrollArea::vertical().id_salt("cleanup-log").max_height(120.0).show(ui, |ui| {
                    for line in cleanup.log.iter().rev() {
                        ui.label(RichText::new(line).color(p.muted).size(11.0));
                    }
                });
            });

        if let Some(action) = action {
            self.send(action);
        }
        if remove && let Some(rows) = &cleanup.rows {
            let worktrees: Vec<PathBuf> = rows
                .iter()
                .filter(|r| self.client.cleanup_selected.contains(&r.path))
                .map(|r| r.path.clone())
                .collect();
            let orphans = cleanup
                .sandboxes
                .orphans
                .iter()
                .filter(|o| self.client.cleanup_sandboxes.contains(&o.name) && !live_sandboxes.contains(&o.name))
                .map(|o| o.name.clone())
                .collect();
            cleanup.busy = Some(format!("removing {} worktrees…", worktrees.len()));
            let delete_branches = self.client.cleanup_delete_branches;
            self.send(Command::RemoveWorktrees { worktrees, orphans, delete_branches });
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
    use super::super::tests::{repository, with_login_state, with_state};
    use super::*;

    /// Started from a desktop launcher, ATOMICS has a bare PATH; git-lfs and Codex (a script for an
    /// interpreter, as Node runs Codex) are in a folder only the login shell adds.
    #[test]
    fn from_a_bare_launcher_path_dispatch_and_codex_use_the_login_shell_path() {
        with_login_state(
            "app::worktrees::tests::from_a_bare_launcher_path_dispatch_and_codex_use_the_login_shell_path",
            |home| {
                let tools = home.join("login-tools");
                std::fs::create_dir(&tools).unwrap();
                let program = |name: &str, text: &str| crate::test_support::write_program(tools.join(name), text);
                program(
                    "git-lfs",
                    "#!/bin/sh\ncase \"$1\" in clean|smudge) exec cat ;; post-*) exit 0 ;; esac\nexit 1\n",
                );
                program("atomics-test-node", "#!/bin/sh\nexec /bin/sh \"$@\"\n");
                program(
                    "codex",
                    "#!/usr/bin/env atomics-test-node\nif [ \"$1\" = --version ]; then printf 'codex-cli 0.165.0\\n'; \
                     else printf '%s\\n' \"$@\" > launch-args; fi\n",
                );
                // Git and the rest stay where this test found them.
                let login = format!("{}:{}", tools.display(), std::env::var("PATH").unwrap());
                let shell = home.join("login-shell");
                crate::test_support::write_program(
                    &shell,
                    format!(
                        "#!/bin/sh\n[ \"$1 $2 $3\" = '-l -i -c' ] || exit 64\nPATH='{login}'\nexport PATH\neval \"$4\"\n"
                    ),
                );
                vec![("SHELL", shell.into()), ("PATH", "/usr/bin:/bin".into())]
            },
            |mut state, _ctx| {
                let tools = paths::home().join("login-tools");
                let path = std::env::var("PATH").unwrap();
                assert!(path.starts_with(&format!("{}:", tools.display())), "{path}");
                for started in ["/usr/bin", "/bin"] {
                    assert!(path.split(':').any(|folder| folder == started), "{started} stays: {path}");
                }
                assert!(crate::setup::login_path().added.is_some_and(|added| added > 0));

                let (_dir, repo) = repository();
                // Not named `lfs`: `git lfs install --system`, as on CI's machines, gives that filter a
                // `process` command, which git prefers and this fake does not speak.
                for (key, value) in [
                    ("filter.test-lfs.clean", "git-lfs clean -- %f"),
                    ("filter.test-lfs.smudge", "git-lfs smudge -- %f"),
                    ("filter.test-lfs.required", "true"),
                ] {
                    git::git(&repo, &["config", key, value]).unwrap();
                }
                // As `git lfs install` writes it.
                crate::test_support::write_program(
                    repo.join(".git/hooks/post-checkout"),
                    "#!/bin/sh\ncommand -v git-lfs >/dev/null 2>&1 || { echo >&2 \"git-lfs was not found on your path\"; \
                     exit 2; }\ngit lfs post-checkout \"$@\"\n",
                );
                std::fs::write(repo.join(".gitattributes"), "*.bin filter=test-lfs diff=lfs merge=lfs -text\n")
                    .unwrap();
                std::fs::write(repo.join("asset.bin"), "large file\n").unwrap();
                git::git(&repo, &["add", "."]).unwrap();
                git::git(&repo, &["commit", "-qm", "asset"]).unwrap();

                let codex = crate::setup::resolve_program("codex");
                assert_eq!(codex, tools.join("codex").to_string_lossy());
                assert_eq!(crate::setup::codex_supports_hooks(&codex, None, Duration::from_secs(10)), Some(true));

                state.config.base_ref = "main".into();
                state.config.agents.push(atomics_core::config::AgentConfig {
                    name: "login-codex".into(),
                    command: vec!["codex".into()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                let dispatch = |state: &mut State, branch: &str| {
                    let launch = Launch {
                        agent: "login-codex".into(),
                        prompt: None,
                        args: vec![],
                        reply: None,
                        task: None,
                        isolation: None,
                        interactive: false,
                    };
                    state.create_worktree(repo.clone(), branch.into(), None, launch);
                    state
                        .worktree_rx
                        .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(30)))
                        .unwrap()
                };

                // A required filter that cannot run fails the dispatch, which leaves nothing behind.
                git::git(&repo, &["config", "filter.test-lfs.smudge", "atomics-test-missing-lfs smudge -- %f"])
                    .unwrap();
                let failed = dispatch(&mut state, "task");
                let error = failed.1.as_ref().err().expect("the dispatch fails");
                assert!(error.contains("smudge filter test-lfs failed"), "{error}");
                assert!(!git::branch_ref_exists(&repo, "task").unwrap(), "a retry would find the branch");
                assert_eq!(git::list_worktrees(&repo).unwrap().len(), 1);

                // The retry, with the filter git-lfs, checks the asset out and starts Codex with hooks.
                git::git(&repo, &["config", "filter.test-lfs.smudge", "git-lfs smudge -- %f"]).unwrap();
                state.worktree_tx.send(failed).unwrap();
                state.runtime_tick();
                let event = dispatch(&mut state, "task");
                let worktree = event.1.as_ref().unwrap().path.clone();
                assert_eq!(std::fs::read_to_string(worktree.join("asset.bin")).unwrap(), "large file\n");
                state.worktree_tx.send(event).unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                while !state
                    .sessions
                    .iter()
                    .any(|s| s.cwd == worktree && s.terminal.as_ref().is_some_and(Terminal::exited))
                {
                    assert!(Instant::now() < deadline, "Codex never started in the worktree");
                    state.runtime_tick();
                    std::thread::sleep(Duration::from_millis(10));
                }
                let args = std::fs::read_to_string(worktree.join("launch-args")).unwrap();
                assert!(args.contains("--no-daemon") && args.contains("hooks.PermissionRequest"), "{args}");
                assert!(state.sessions.iter().all(|s| s.warning.is_none()));
                state.sessions.clear();
            },
        );
    }

    #[test]
    fn cache_warming_finishes_on_the_worktree_job_before_agent_launch() {
        with_state(
            "app::worktrees::tests::cache_warming_finishes_on_the_worktree_job_before_agent_launch",
            |mut state, _ctx| {
                let (_dir, repo) = repository();
                state.config.base_ref = "main".into();
                state.config.agents.push(atomics_core::config::AgentConfig {
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
                let config_before = std::fs::read(repo.join(".git/config")).unwrap();
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
                        Launch {
                            agent: "probe".into(),
                            prompt: None,
                            args: vec![],
                            reply: None,
                            task: None,
                            isolation: None,
                            interactive: false,
                        },
                    );
                    let event = state
                        .worktree_rx
                        .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                        .unwrap();
                    let prepared = event.1.as_ref().unwrap();
                    let path = prepared.path.clone();
                    assert_eq!(prepared.warmed, branch == "warm" && supported);
                    assert!(!path.join("launched").exists());
                    if branch == "failed" {
                        assert!(started.elapsed() < Duration::from_secs(2));
                        assert!(!path.join("target").exists());
                    }
                    state.worktree_tx.send(event).unwrap();
                    state.runtime_tick();
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                    while !path.join("launched").exists() {
                        assert!(Instant::now() < deadline, "agent never launched for {branch}");
                        state.runtime_tick();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if branch == "warm" && supported {
                        assert_eq!(state.toast.as_ref().unwrap().0, "warm cache from main");
                    }
                    assert_eq!(std::fs::read_to_string(path.join("launched")).unwrap(), expected);
                    assert_eq!(
                        std::fs::read(repo.join(".git/config")).unwrap(),
                        config_before,
                        "unconfigured worktrees must not receive identity includes"
                    );
                    assert!(!repo.join(".git/atomics-identities").exists());
                }
            },
        );
    }
}
