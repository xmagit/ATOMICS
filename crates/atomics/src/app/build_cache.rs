//! Disk sampling and cleanup jobs stay off the runtime/UI lock.
use super::*;
use atomics_core::cache::CacheConfig;
use atomics_core::machine::{BuildOutput, DiskWarning};

type Sample = (Vec<DiskWarning>, Vec<BuildOutput>);

#[derive(Default)]
pub(super) struct Caches {
    checked: Option<Instant>,
    reading: Option<Receiver<Sample>>,
    largest: Vec<BuildOutput>,
    cleanup: HashMap<PathBuf, CacheConfig>,
    cleaning: bool,
    cleanup_at: Option<Instant>,
    attempted: HashMap<PathBuf, Instant>,
}

impl State {
    fn cache_worktrees(&self) -> Vec<(PathBuf, CacheConfig)> {
        let mut found = HashMap::new();
        for repo in &self.repos {
            let config = self.project_config(repo).cache;
            if let Ok(entries) = git::list_worktrees(repo) {
                for entry in entries.into_iter().skip(1).filter(|entry| entry.trusted) {
                    found.insert(entry.path, config.clone());
                }
            }
        }
        found.into_iter().collect()
    }

    pub(super) fn check_disk(&self, path: &Path) -> Result<(), String> {
        if let Some(disk) = crate::build_cache::disk(path)
            && let Some(mut warning) = DiskWarning::of(
                &disk,
                &self.config.resources,
                self.build_caches
                    .largest
                    .iter()
                    .filter(|folder| {
                        crate::build_cache::device(Path::new(&folder.path)) == crate::build_cache::device(path)
                    })
                    .cloned()
                    .collect(),
            )
            && warning.blocked
        {
            if warning.largest.is_empty() {
                warning.largest = self
                    .cache_worktrees()
                    .into_iter()
                    .filter(|(root, _)| crate::build_cache::device(root) == crate::build_cache::device(path))
                    .flat_map(|(root, cache)| crate::build_cache::inventory(&root, &cache))
                    .collect();
                warning.largest.sort_by_key(|folder| std::cmp::Reverse(folder.bytes));
                warning.largest.truncate(5);
            }
            return Err(warning.message());
        }
        Ok(())
    }

    pub(super) fn schedule_cache_cleanup(&mut self, path: &Path) {
        if self.build_caches.attempted.get(path).is_none_or(|at| at.elapsed() >= Duration::from_secs(30)) {
            self.build_caches.cleanup.insert(path.to_path_buf(), self.project_config(path).cache);
        }
    }

    pub(super) fn clean_build_caches(&mut self) {
        for (path, config) in self.cache_worktrees() {
            self.build_caches.cleanup.insert(path, config);
        }
        self.poll_build_caches();
        self.toast("Cleaning build caches of idle worktrees…");
    }

    pub(super) fn poll_build_caches(&mut self) -> bool {
        let mut changed = false;
        if let Some(sample) = self.build_caches.reading.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.resources.disk = sample.0;
            self.build_caches.largest = sample.1;
            self.build_caches.reading = None;
            changed = true;
        }
        if self.build_caches.reading.is_none()
            && self.build_caches.checked.is_none_or(|at| at.elapsed() >= Duration::from_secs(10))
        {
            self.build_caches.checked = Some(Instant::now());
            let repos = self.repos.clone();
            let config = self.config.clone();
            let projects: Vec<_> = repos.iter().map(|repo| (repo.clone(), self.project_config(repo).cache)).collect();
            let mut destinations: Vec<_> = self.sessions.iter().map(Session::root).collect();
            for repo in &repos {
                destinations.push(git::worktree_path(&config.worktree_path, repo, "disk-probe"));
            }
            // Pinned task paths may be on a different filesystem from the default template.
            destinations
                .extend(self.plans.iter().flat_map(|plan| plan.tasks.iter().filter_map(|task| task.worktree.clone())));
            let (tx, rx) = self.wake.channel();
            self.build_caches.reading = Some(rx);
            std::thread::spawn(move || {
                let mut worktrees = Vec::new();
                for (repo, cache) in projects {
                    if let Ok(entries) = git::list_worktrees(&repo) {
                        worktrees
                            .extend(entries.into_iter().skip(1).filter(|w| w.trusted).map(|w| (w.path, cache.clone())));
                    }
                }
                destinations.extend(worktrees.iter().map(|(path, _)| path.clone()));
                destinations.sort();
                destinations.dedup();
                let mut devices = HashSet::new();
                let disks: Vec<_> = destinations
                    .iter()
                    .filter(|path| devices.insert(crate::build_cache::device(path)))
                    .filter_map(|path| crate::build_cache::disk(path))
                    .collect();
                let low = disks.iter().any(|disk| DiskWarning::of(disk, &config.resources, vec![]).is_some());
                let mut largest: Vec<_> = if low {
                    worktrees.iter().flat_map(|(path, cache)| crate::build_cache::inventory(path, cache)).collect()
                } else {
                    Vec::new()
                };
                largest.sort_by_key(|folder| std::cmp::Reverse(folder.bytes));
                largest.dedup_by(|a, b| a.path == b.path);
                largest.truncate(5);
                let warnings =
                    disks.iter().filter_map(|disk| DiskWarning::of(disk, &config.resources, largest.clone())).collect();
                let _ = tx.send((warnings, largest));
            });
        }
        if !self.build_caches.cleaning
            && !self.build_caches.cleanup.is_empty()
            && self.build_caches.cleanup_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(10))
        {
            self.build_caches.cleanup_at = Some(Instant::now());
            // A user may have changed output policy while an in-use folder waited for retry.
            let targets: HashMap<_, _> = std::mem::take(&mut self.build_caches.cleanup)
                .into_keys()
                .map(|path| {
                    let cache = self.project_config(&path).cache;
                    (path, cache)
                })
                .collect();
            let protected: HashSet<_> = self
                .sessions
                .iter()
                .filter(|s| {
                    s.sandbox.is_some()
                        || !matches!(s.status, AgentStatus::Idle | AgentStatus::Done | AgentStatus::Exited)
                        || s.screen_busy
                        || s.pending.is_some()
                })
                .map(Session::root)
                .collect();
            for path in targets.keys() {
                self.build_caches.attempted.insert(path.clone(), Instant::now());
            }
            self.build_caches.cleaning = true;
            let jobs = self.running_lifecycle();
            let sbx = resolve_program(&self.sbx);
            let actions = self.action_tx.clone();
            self.lifecycle(
                Scope { sandboxes: vec![], worktrees: Some(targets.keys().cloned().collect()) },
                move || {
                    let mut retry = HashMap::new();
                    for (path, config) in targets {
                        crate::lifecycle::wait_for(&jobs, None, Some(&path));
                        if protected.contains(&path) {
                            retry.insert(path, config);
                            continue;
                        }
                        if crate::sandbox::installed(&sbx)
                            && !crate::sandbox::build_cache_users(&sbx, &path).is_ok_and(|users| users.is_empty())
                        {
                            retry.insert(path, config);
                            continue;
                        }
                        match crate::build_cache::clean(&path, &config) {
                            Ok(notes) => {
                                for note in notes {
                                    eprintln!("ATOMICS: {note}");
                                }
                            }
                            Err(error) if path.exists() && error.to_string().contains("in use") => {
                                eprintln!("ATOMICS: kept build output in {}: {error:#}", path.display());
                                retry.insert(path, config);
                            }
                            Err(error) => {
                                eprintln!("ATOMICS: kept build output in {}: {error:#}", path.display());
                            }
                        }
                    }
                    let _ = actions.send(Box::new(move |state| {
                        state.build_caches.cleaning = false;
                        state.build_caches.cleanup.extend(retry);
                        state.build_caches.checked = Some(Instant::now());
                    }));
                },
            );
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, session, with_app};
    use super::*;

    #[test]
    fn agent_sessions_inherit_lean_build_cache_defaults_and_config_overrides() {
        with_app(
            "app::build_cache::tests::agent_sessions_inherit_lean_build_cache_defaults_and_config_overrides",
            |app, _| {
                let (dir, repo) = repository();
                std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                let output = dir.path().join("agent-env");
                app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
                app.runtime.config.cache.env.insert("CARGO_PROFILE_DEV_DEBUG".into(), "full".into());
                app.runtime.config.cache.env.insert("RUSTC_WRAPPER".into(), String::new());
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "cache-probe".into(),
                    command: vec![
                        "sh".into(),
                        "-c".into(),
                        "env > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"; sleep 60".into(),
                        "cache-probe".into(),
                        output.to_string_lossy().into_owned(),
                    ],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                let id = app.runtime.start("cache-probe", repo, None, vec![], None).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !output.exists() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                let env = std::fs::read_to_string(output).unwrap();
                for expected in [
                    "CARGO_INCREMENTAL=0",
                    "CARGO_PROFILE_DEV_DEBUG=full",
                    "CARGO_PROFILE_TEST_DEBUG=line-tables-only",
                    "RUSTC_WRAPPER=",
                ] {
                    assert!(env.lines().any(|line| line == expected), "missing {expected}");
                }
                app.runtime.close(&id);
            },
        );
    }

    #[test]
    fn low_disk_refuses_start_and_dispatch_and_publishes_the_cleanup_action() {
        with_app(
            "app::build_cache::tests::low_disk_refuses_start_and_dispatch_and_publishes_the_cleanup_action",
            |app, ctx| {
                let (_dir, repo) = repository();
                app.runtime.config.resources.disk_min_gib = u64::MAX;
                app.runtime.config.resources.disk_warn_gib = u64::MAX;
                let error = app.runtime.start("shell", repo.clone(), None, vec![], None).unwrap_err();
                assert!(error.contains("New tasks and sessions are paused"));
                assert!(error.contains(atomics_core::machine::CLEAN_BUILD_CACHES));
                assert!(app.runtime.sessions.is_empty());
                app.runtime.plans = vec![Plan {
                    id: "disk".into(),
                    project: Some(repo.clone()),
                    approved: true,
                    tasks: vec![Task { id: "task".into(), agent: "shell".into(), ..Default::default() }],
                    ..Default::default()
                }];
                assert!(app.runtime.dispatch(0, None).unwrap_err().contains("Low disk space"));
                assert_eq!(git::list_worktrees(&repo).unwrap().len(), 1);
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Planned);
                app.runtime.repos = vec![repo];
                app.runtime.poll_build_caches();
                let deadline = Instant::now() + Duration::from_secs(10);
                while app.runtime.build_caches.reading.is_some() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                    app.runtime.poll_build_caches();
                }
                assert!(app.runtime.resources.disk[0].blocked);
                let list = app.runtime.resources_json();
                assert_eq!(list["cleanup"]["tool"], "atomics_clean_build_caches");
                assert_eq!(list["disk"][0]["blocked"], true);
                let backend = app.runtime.backend();
                let palette = app.client.palette.clone();
                let output = ctx.run_ui(Default::default(), |ui| {
                    atomics_ui::resources::load(ui, &backend, &palette, false);
                    atomics_ui::resources::disk_warning(ui, &backend, app.client);
                });
                let texts: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) => Some(text),
                        _ => None,
                    })
                    .collect();
                assert!(texts.iter().any(|text| text.galley.text() == atomics_core::machine::CLEAN_BUILD_CACHES));
                assert!(
                    texts
                        .iter()
                        .filter(|text| text.galley.text().starts_with("load "))
                        .flat_map(|text| text.galley.rows.iter())
                        .flat_map(|row| &row.visuals.mesh.vertices)
                        .any(|vertex| vertex.color == palette.warning)
                );
                output.drop_without_applying_deltas();
            },
        );
    }

    #[test]
    fn acceptance_and_session_close_clean_outputs_while_worktrees_remain() {
        with_app(
            "app::build_cache::tests::acceptance_and_session_close_clean_outputs_while_worktrees_remain",
            |app, _| {
                for accepted in [false, true] {
                    let (_dir, repo) = repository();
                    std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
                    git::git(&repo, &["add", "."]).unwrap();
                    git::git(&repo, &["commit", "-qm", "workspace"]).unwrap();
                    let work = repo.with_file_name("task");
                    git::add_worktree(&repo, &work, "task", "main").unwrap();
                    std::fs::create_dir(work.join("target")).unwrap();
                    std::fs::write(work.join("target/artifact"), "compiled").unwrap();
                    app.runtime.config.cleanup.completed_plans = false;
                    app.runtime.config.base_ref = "main".into();
                    app.runtime.worktree_project.insert(work.clone(), repo);
                    app.runtime.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Done, &work)];
                    if accepted {
                        app.runtime.plans = vec![Plan {
                            id: "accepted".into(),
                            tasks: vec![Task {
                                id: "task".into(),
                                session: Some("worker".into()),
                                status: TaskStatus::Done,
                                integrated: Some("integration".into()),
                                integration_verified: true,
                                ..Default::default()
                            }],
                            integration: Some("integration".into()),
                            ..Default::default()
                        }];
                        super::super::agent_cleanup::finish_checks(app.runtime);
                        assert!(app.runtime.sessions.is_empty());
                    } else {
                        app.runtime.plans.clear();
                        app.runtime.close("worker");
                    }
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while work.join("target").exists() {
                        assert!(Instant::now() < deadline, "build output was not cleaned");
                        app.runtime.poll_build_caches();
                        app.runtime.finish_lifecycle_jobs();
                        app.runtime.drain_actions();
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    assert!(work.join("Cargo.toml").is_file());
                    app.runtime.build_caches.cleanup_at = None;
                }
            },
        );
    }
}
