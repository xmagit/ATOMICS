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
    cleanup: HashMap<PathBuf, PendingCleanup>,
    cleaning: bool,
    cleanup_at: Option<Instant>,
    cleaned: HashSet<PathBuf>,
    inventory_at: Option<Instant>,
    inventory_after_cleanup: bool,
    manual_skipped: Vec<PathBuf>,
}

#[derive(Clone)]
struct PendingCleanup {
    attempts: usize,
    due: Instant,
    manual: bool,
}

impl PendingCleanup {
    fn new(manual: bool) -> Self {
        Self { attempts: 0, due: Instant::now(), manual }
    }

    fn after_in_use(&self) -> Option<Self> {
        // Five attempts total, over roughly two minutes. A manual batch never
        // queues work for later: it promises to clean only currently idle worktrees.
        let delays = [5, 15, 30, 60];
        let delay = (!self.manual).then(|| delays.get(self.attempts)).flatten()?;
        Some(Self { attempts: self.attempts + 1, due: Instant::now() + Duration::from_secs(*delay), manual: false })
    }
}

fn cleanup_notice(skipped: &[PathBuf]) -> String {
    if skipped.is_empty() {
        "Cleaning build caches of idle worktrees…".into()
    } else {
        format!(
            "Cleaning idle worktrees. Skipped active or protected worktrees: {}. Close their sessions before retrying.",
            skipped.iter().map(|path| paths::display(path)).collect::<Vec<_>>().join(", ")
        )
    }
}

fn outputs_for_device(
    outputs: &[BuildOutput],
    device: Option<u64>,
    device_of: impl Fn(&str) -> Option<u64>,
) -> Vec<BuildOutput> {
    let mut matching: Vec<_> =
        outputs.iter().filter(|output| device.is_some() && device_of(&output.path) == device).cloned().collect();
    matching.sort_by_key(|output| std::cmp::Reverse(output.bytes));
    // Always expose refused folders and their reasons, including unmeasurable
    // ones, alongside the five largest eligible outputs.
    let mut eligible = 0;
    matching.retain(|output| {
        if output.reason.is_some() {
            true
        } else {
            eligible += 1;
            eligible <= 5
        }
    });
    matching
}

fn protected_worktree(protected: &HashSet<PathBuf>, path: &Path) -> bool {
    protected.iter().any(|busy| busy.starts_with(path) || path.starts_with(busy))
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
            && let Some(warning) = DiskWarning::of(
                &disk,
                &self.config.resources,
                outputs_for_device(&self.build_caches.largest, crate::build_cache::device(path), |path| {
                    crate::build_cache::device(Path::new(path))
                }),
            )
            && warning.blocked
        {
            return Err(warning.message());
        }
        Ok(())
    }

    pub(super) fn session_disk_guard(&self, path: &Path, chief: bool, resume: bool, shell: bool) -> Result<(), String> {
        match self.check_disk(path) {
            Err(warning) if chief || resume || shell => {
                eprintln!(
                    "ATOMICS: {warning} Continuing {}.",
                    if shell {
                        "the shell"
                    } else if chief {
                        "the chief"
                    } else {
                        "the resumed session"
                    }
                );
                Ok(())
            }
            result => result,
        }
    }

    pub(super) fn cancel_cache_cleanup(&mut self, path: &Path) {
        let root = crate::build_cache::root(path);
        let root = root.canonicalize().unwrap_or(root);
        self.build_caches
            .cleanup
            .retain(|candidate, _| candidate.canonicalize().as_ref().unwrap_or(candidate) != &root);
        self.build_caches.cleaned.retain(|candidate| candidate.canonicalize().as_ref().unwrap_or(candidate) != &root);
    }

    pub(super) fn schedule_cache_cleanup(&mut self, path: &Path) {
        let path = crate::build_cache::root(path);
        let path = path.canonicalize().unwrap_or(path);
        if !self.build_caches.cleaned.contains(&path) {
            self.build_caches.cleanup.entry(path).or_insert_with(|| PendingCleanup::new(false));
        }
    }

    fn cache_protected(&self) -> HashSet<PathBuf> {
        self.sessions
            .iter()
            .filter(|s| {
                s.sandbox.is_some()
                    || !matches!(s.status, AgentStatus::Idle | AgentStatus::Done | AgentStatus::Exited)
                    || s.screen_busy
                    || s.pending.is_some()
                    || s.terminal.as_ref().is_some_and(|terminal| !terminal.exited())
            })
            .map(|session| {
                let root = session.root();
                root.canonicalize().unwrap_or(root)
            })
            .chain(self.resume.iter().map(|record| crate::build_cache::root(&record.cwd)))
            .collect()
    }

    pub(super) fn clean_build_caches(&mut self) {
        let protected = self.cache_protected();
        // Busy or protected entries are discarded by this batch, never retried later.
        let mut skipped = Vec::new();
        for (path, _) in self.cache_worktrees() {
            if protected_worktree(&protected, &path) {
                skipped.push(path);
            } else {
                self.build_caches.cleanup.entry(path).or_insert_with(|| PendingCleanup::new(true));
            }
        }
        skipped.sort();
        let queued = self.build_caches.cleanup.values().any(|pending| pending.manual);
        self.build_caches.manual_skipped = if queued { skipped.clone() } else { Vec::new() };
        self.build_caches.inventory_after_cleanup = queued;
        self.poll_build_caches();
        self.toast(cleanup_notice(&skipped));
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
            let inventory_due =
                self.build_caches.inventory_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(120));
            if inventory_due {
                self.build_caches.inventory_at = Some(Instant::now());
            }
            let previous = self.build_caches.largest.clone();
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
                let mut largest: Vec<_> =
                    if low && inventory_due { crate::build_cache::inventories(&worktrees) } else { previous };
                largest.sort_by_key(|folder| std::cmp::Reverse(folder.bytes));
                largest.dedup_by(|a, b| a.path == b.path);
                let warnings = disks
                    .iter()
                    .filter_map(|disk| {
                        DiskWarning::of(
                            disk,
                            &config.resources,
                            outputs_for_device(&largest, crate::build_cache::device(Path::new(&disk.path)), |path| {
                                crate::build_cache::device(Path::new(path))
                            }),
                        )
                    })
                    .collect();
                let _ = tx.send((warnings, largest));
            });
        }
        if !self.build_caches.cleaning
            && self.build_caches.cleanup.values().any(|pending| pending.due <= Instant::now())
            && self.build_caches.cleanup_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(10))
        {
            self.build_caches.cleanup_at = Some(Instant::now());
            // Refresh policy on every attempt. Future retries remain cancellable
            // by a session start and never hold a lifecycle scope while waiting.
            let now = Instant::now();
            let due: Vec<_> = self
                .build_caches
                .cleanup
                .iter()
                .filter(|(_, pending)| pending.due <= now)
                .map(|(path, _)| path.clone())
                .collect();
            let protected = self.cache_protected();
            let mut targets = Vec::new();
            let mut skipped = std::mem::take(&mut self.build_caches.manual_skipped);
            for path in due {
                let pending = self.build_caches.cleanup.remove(&path).unwrap();
                if protected_worktree(&protected, &path) {
                    if pending.manual {
                        skipped.push(path);
                    }
                    continue;
                }
                let cache = self.project_config(&path).cache;
                targets.push((path, cache, pending));
            }
            if targets.is_empty() {
                self.build_caches.inventory_after_cleanup = false;
                if !skipped.is_empty() {
                    self.toast(cleanup_notice(&skipped));
                }
                return changed;
            }
            let manual = targets.iter().any(|(_, _, pending)| pending.manual);
            let refresh_inventory = std::mem::take(&mut self.build_caches.inventory_after_cleanup);
            self.build_caches.cleaning = true;
            let jobs = self.running_lifecycle();
            let sbx = resolve_program(&self.sbx);
            let actions = self.action_tx.clone();
            self.lifecycle(
                Scope { sandboxes: vec![], worktrees: Some(targets.iter().map(|(path, _, _)| path.clone()).collect()) },
                move || {
                    let mut cleaned = HashSet::new();
                    let mut retries = HashMap::new();
                    let mut partial = Vec::new();
                    for (path, config, pending) in targets {
                        crate::lifecycle::wait_for(&jobs, None, Some(&path));
                        if crate::sandbox::installed(&sbx)
                            && !crate::sandbox::build_cache_users(&sbx, &path).is_ok_and(|users| users.is_empty())
                        {
                            if pending.manual {
                                skipped.push(path);
                            } else if let Some(retry) = pending.after_in_use() {
                                retries.insert(path, retry);
                            }
                            continue;
                        }
                        match crate::build_cache::clean(&path, &config) {
                            Ok(notes) => {
                                cleaned.insert(path);
                                for note in notes {
                                    eprintln!("ATOMICS: {note}");
                                }
                            }
                            Err(error) => {
                                if let Some(notice) = crate::build_cache::partial_removal(&error) {
                                    eprintln!("ATOMICS: {notice}");
                                    partial.push(notice);
                                } else {
                                    eprintln!("ATOMICS: kept build output in {}: {error:#}", path.display());
                                }
                                if error.to_string().contains("in use") {
                                    if pending.manual {
                                        skipped.push(path);
                                    } else if let Some(retry) = pending.after_in_use() {
                                        retries.insert(path, retry);
                                    }
                                }
                            }
                        }
                    }
                    let _ = actions.send(Box::new(move |state| {
                        state.build_caches.cleaning = false;
                        state.build_caches.cleaned.extend(cleaned);
                        // A newly started session cancels even a retry returned by
                        // a worker that was finishing concurrently with that start.
                        let protected = state.cache_protected();
                        for (path, retry) in retries {
                            if !protected_worktree(&protected, &path) {
                                state.build_caches.cleanup.entry(path).or_insert(retry);
                            }
                        }
                        if manual && !skipped.is_empty() {
                            skipped.sort();
                            skipped.dedup();
                            state.toast(cleanup_notice(&skipped));
                        }
                        if !partial.is_empty() {
                            state.toast(partial.join("; "));
                        }
                        // Automatic cleanup refreshes free space, keeping the 120 s tree-walk
                        // limit. Only a manual request asks for a fresh folder inventory.
                        state.build_caches.checked = None;
                        if refresh_inventory {
                            state.build_caches.inventory_at = None;
                        }
                    }));
                },
            );
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{persist_config, repository, session, wait_started, with_app};
    use super::*;

    #[test]
    fn agent_sessions_inherit_lean_build_cache_defaults_and_config_overrides() {
        with_app(
            "app::build_cache::tests::agent_sessions_inherit_lean_build_cache_defaults_and_config_overrides",
            |app, _| {
                let (dir, repo) = repository();
                std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                git::git(&repo, &["add", "Cargo.toml"]).unwrap();
                git::git(&repo, &["commit", "-qm", "workspace"]).unwrap();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
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
                persist_config(app.runtime);
                let id = app.runtime.start("cache-probe", work, None, vec![], None).unwrap();
                wait_started(app.runtime, &id);
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
    fn foreign_or_legacy_cache_links_never_refuse_a_bare_resume_and_other_folders_still_work() {
        with_app(
            "app::build_cache::tests::foreign_or_legacy_cache_links_never_refuse_a_bare_resume_and_other_folders_still_work",
            |app, _| {
                for namespace in ["repos", "sandbox-repos-v2"] {
                    let (dir, repo) = repository();
                    std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                    git::git(&repo, &["add", "Cargo.toml"]).unwrap();
                    git::git(&repo, &["commit", "-qm", "workspace"]).unwrap();
                    let work = dir.path().join("task");
                    git::add_worktree(&repo, &work, "task", "main").unwrap();
                    let store = paths::atomics_home().join("build-caches").join(namespace).join("old/packages");
                    std::fs::create_dir_all(&store).unwrap();
                    std::fs::write(store.join("package"), "keep").unwrap();
                    std::fs::create_dir_all(work.join(".nuget")).unwrap();
                    std::os::unix::fs::symlink(&store, work.join(".nuget/packages")).unwrap();
                    app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
                    app.runtime.config.cache.shared = vec![".nuget/packages".into(), ".cache/pip".into()];
                    app.runtime.config.cache.env.insert("NUGET_PACKAGES".into(), ".nuget/packages".into());
                    app.runtime.config.cache.env.insert("PIP_CACHE_DIR".into(), ".cache/pip".into());
                    app.runtime.config.cache.env.insert("RUSTC_WRAPPER".into(), String::new());
                    let output = dir.path().join("env");
                    let agent = format!("cache-{namespace}");
                    app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                        name: agent.clone(),
                        command: vec![
                            "sh".into(),
                            "-c".into(),
                            format!(
                                "env > '{}.tmp'; mv '{}.tmp' '{}'; sleep 60",
                                output.display(),
                                output.display(),
                                output.display()
                            ),
                        ],
                        kind: AgentKind::Generic,
                        busy_text: None,
                        isolation: None,
                    });
                    persist_config(app.runtime);
                    let id =
                        app.runtime.start(&agent, work.clone(), Some("conversation".into()), vec![], None).unwrap();
                    wait_started(app.runtime, &id);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !output.exists() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let env = std::fs::read_to_string(&output).unwrap();
                    assert!(!env.lines().any(|line| line.starts_with("NUGET_PACKAGES=")));
                    assert!(env.lines().any(|line| line.starts_with("PIP_CACHE_DIR=")));
                    assert!(work.join(".cache/pip").symlink_metadata().unwrap().file_type().is_symlink());
                    assert!(work.join(".nuget/packages").symlink_metadata().is_err());
                    assert_eq!(std::fs::read_to_string(store.join("package")).unwrap(), "keep");
                    app.runtime.close_for_resume(&id);
                }
            },
        );
    }

    #[test]
    fn cleanup_retries_an_exiting_real_child_and_then_reclaims_output() {
        with_app(
            "app::build_cache::tests::cleanup_retries_an_exiting_real_child_and_then_reclaims_output",
            |app, _| {
                use std::io::Write;
                use std::process::{Command, Stdio};
                let (dir, repo) = repository();
                std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
                git::git(&repo, &["add", "."]).unwrap();
                git::git(&repo, &["commit", "-qm", "workspace"]).unwrap();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                std::fs::create_dir(work.join("target")).unwrap();
                std::fs::write(work.join("target/artifact"), "compiled").unwrap();
                let mut child = Command::new(crate::test_support::program("sh"))
                    .args(["-c", "read finish"])
                    .current_dir(&work)
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap();
                app.runtime.sessions = vec![session("closed", AgentKind::Generic, AgentStatus::Done, &work)];
                app.runtime.close("closed");
                app.runtime.poll_build_caches();
                app.runtime.finish_lifecycle_jobs();
                app.runtime.drain_actions();
                let key = work.canonicalize().unwrap();
                assert_eq!(app.runtime.build_caches.cleanup[&key].attempts, 1);
                assert!(work.join("target/artifact").is_file());
                app.runtime.repos = vec![repo];
                app.runtime.clean_build_caches();
                assert!(
                    !app.runtime.build_caches.cleanup[&key].manual,
                    "a manual request must not cancel an existing automatic retry"
                );
                assert_eq!(app.runtime.build_caches.cleanup[&key].attempts, 1);
                child.stdin.take().unwrap().write_all(b"done\n").unwrap();
                assert!(child.wait().unwrap().success());
                app.runtime.build_caches.cleanup.get_mut(&key).unwrap().due = Instant::now();
                app.runtime.build_caches.cleanup_at = None;
                app.runtime.poll_build_caches();
                app.runtime.finish_lifecycle_jobs();
                app.runtime.drain_actions();
                assert!(!work.join("target").exists());
                assert!(app.runtime.build_caches.cleanup.is_empty());
                assert!(app.runtime.build_caches.cleaned.contains(&key));
            },
        );
    }

    #[test]
    fn cleanup_backoff_is_bounded_and_manual_batches_never_retry_busy_worktrees() {
        let start = Instant::now();
        let mut pending = PendingCleanup::new(false);
        for (index, delay) in [5, 15, 30, 60].into_iter().enumerate() {
            pending = pending.after_in_use().unwrap();
            assert_eq!(pending.attempts, index + 1);
            assert!(pending.due >= start + Duration::from_secs(delay));
        }
        assert!(pending.after_in_use().is_none());
        assert!(PendingCleanup::new(true).after_in_use().is_none());
    }

    #[test]
    fn low_disk_allows_shells_and_cleanup_names_protected_worktrees() {
        with_app("app::build_cache::tests::low_disk_allows_shells_and_cleanup_names_protected_worktrees", |app, _| {
            let (dir, repo) = repository();
            let work = dir.path().join("task");
            git::add_worktree(&repo, &work, "task", "main").unwrap();
            app.runtime.config.resources.disk_min_gib = u64::MAX;
            app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
            persist_config(app.runtime);
            let id = app.runtime.start("shell", work.clone(), None, vec![], None).unwrap();
            wait_started(app.runtime, &id);
            assert!(app.runtime.sessions[app.runtime.find(&id).unwrap()].terminal.is_some());
            app.runtime.repos = vec![repo];
            app.runtime.clean_build_caches();
            assert!(app.runtime.build_caches.cleanup.is_empty());
            let notice = cleanup_notice(&[work]);
            assert!(notice.contains("task"));
            assert!(notice.contains("Close their sessions"));
            app.runtime.close_for_resume(&id);
        });
    }

    #[test]
    fn sandbox_preparation_in_a_subfolder_waits_on_the_worktree_root() {
        with_app("app::build_cache::tests::sandbox_preparation_in_a_subfolder_waits_on_the_worktree_root", |app, _| {
            let (dir, repo) = repository();
            let work = dir.path().join("task");
            git::add_worktree(&repo, &work, "task", "main").unwrap();
            let subdir = work.join("subdir");
            std::fs::create_dir(&subdir).unwrap();
            let bin = crate::test_support::tempdir().unwrap();
            app.runtime.sbx = crate::sandbox::fake::install(bin.path()).to_string_lossy().into_owned();
            std::fs::write(bin.path().join("run-waits"), "").unwrap();
            app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Sandbox;
            let (release, wait) = channel();
            app.runtime.lifecycle(Scope { sandboxes: vec![], worktrees: Some(vec![work.clone()]) }, move || {
                let _ = wait.recv_timeout(Duration::from_secs(10));
            });
            let id = app.runtime.start("codex", subdir, None, vec![], None).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            assert!(crate::sandbox::fake::calls_to(bin.path(), &["create"]).is_empty());
            assert!(crate::sandbox::fake::calls_to(bin.path(), &["exec"]).is_empty());
            release.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                app.runtime.runtime_tick();
                if app.runtime.find(&id).is_some_and(|i| app.runtime.sessions[i].terminal.is_some()) {
                    break;
                }
                assert!(Instant::now() < deadline, "sandbox did not start after cleanup ended");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!crate::sandbox::fake::calls_to(bin.path(), &["create"]).is_empty());
            app.runtime.close_for_resume(&id);
        });
    }

    #[test]
    fn disk_warning_folders_are_sorted_and_limited_per_filesystem() {
        let mut outputs: Vec<_> =
            (0..8).map(|i| BuildOutput { path: format!("disk-a/{i}"), bytes: i, reason: None }).collect();
        outputs.push(BuildOutput { path: "disk-b/large".into(), bytes: 100, reason: None });
        let device = |path: &str| Some(if path.starts_with("disk-a/") { 1 } else { 2 });
        let a = outputs_for_device(&outputs, Some(1), device);
        assert_eq!(a.len(), 5);
        assert_eq!(a[0].bytes, 7);
        assert!(a.iter().all(|output| output.path.starts_with("disk-a/")));
        assert_eq!(outputs_for_device(&outputs, Some(2), device), [outputs[8].clone()]);
        assert!(outputs_for_device(&outputs, None, device).is_empty());
    }

    #[test]
    fn disk_warning_keeps_refused_outputs_and_reasons_even_without_a_size() {
        let mut outputs: Vec<_> =
            (0..8).map(|i| BuildOutput { path: format!("work/target-{i}"), bytes: 100 + i, reason: None }).collect();
        outputs.push(BuildOutput {
            path: "work/target".into(),
            bytes: 0,
            reason: Some("output contains a nested repository; size unavailable".into()),
        });
        let folders = outputs_for_device(&outputs, Some(1), |_| Some(1));
        assert_eq!(folders.len(), 6);
        let warning = DiskWarning { largest: folders, ..Default::default() }.message();
        assert!(warning.contains("work/target; kept: output contains a nested repository; size unavailable"));
        assert!(!warning.contains("work/target (0.00 GiB)"));
    }

    #[test]
    fn resume_cancels_pending_cleanup_and_low_disk_keeps_chief_and_restart_sessions() {
        with_app(
            "app::build_cache::tests::resume_cancels_pending_cleanup_and_low_disk_keeps_chief_and_restart_sessions",
            |app, _| {
                let (dir, repo) = repository();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
                app.runtime.config.resources.disk_min_gib = u64::MAX;
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "resume-probe".into(),
                    command: vec!["sh".into(), "-c".into(), "sleep 60".into()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                let mut exited = session("old", AgentKind::Generic, AgentStatus::Exited, &work);
                exited.agent = "resume-probe".into();
                app.runtime.sessions = vec![exited];
                persist_config(app.runtime);
                app.runtime.schedule_cache_cleanup(&work);
                app.runtime.resume_exited("old");
                assert!(app.runtime.build_caches.cleanup.is_empty());
                assert!(app.runtime.lifecycle_jobs.is_empty());
                let record = app.runtime.resume.pop().unwrap();
                app.runtime.resume_record(record);
                assert_eq!(app.runtime.sessions.len(), 1);
                let id = app.runtime.sessions[0].id.clone();
                wait_started(app.runtime, &id);
                assert!(app.runtime.sessions[0].terminal.is_some());
                // A restart closes the terminal but must preserve its output.
                let id = app.runtime.sessions[0].id.clone();
                app.runtime.restarting = true;
                app.runtime.close(&id);
                app.runtime.restarting = false;
                assert!(app.runtime.build_caches.cleanup.is_empty());
                app.runtime.config.chief.command = vec!["sh".into(), "-c".into(), "sleep 60".into()];
                app.runtime.config.chief.args.clear();
                persist_config(app.runtime);
                let chief = app.runtime.start_chief(None).unwrap();
                wait_started(app.runtime, &chief);
                assert!(app.runtime.sessions.iter().any(|session| session.id == chief && session.chief));
                app.runtime.close_for_resume(&chief);
                // Exercise the actual consumed restart handoff under the same disk pressure.
                app.runtime.store.restart = Some(atomics_core::store::RestartState {
                    sessions: vec![SessionRecord {
                        chief: false,
                        pane: Some("prior".into()),
                        claude_session_id: "conversation".into(),
                        agent: "resume-probe".into(),
                        cwd: work.clone(),
                        title: "resumed".into(),
                        last_prompt: None,
                        updated: 0,
                        sandbox: None,
                        named: false,
                    }],
                    ..Default::default()
                });
                app.runtime.restore_restart();
                for record in std::mem::take(&mut app.runtime.resume) {
                    app.runtime.resume_record(record);
                }
                assert!(app.runtime.store.restart.is_none());
                assert_eq!(app.runtime.sessions.len(), 1);
                let id = app.runtime.sessions[0].id.clone();
                wait_started(app.runtime, &id);
                assert_eq!(app.runtime.sessions[0].claude_id.as_deref(), Some("conversation"));
                assert!(app.runtime.sessions[0].terminal.is_some());
            },
        );
    }

    #[test]
    fn resume_checks_running_cleanup_before_closing_and_sandbox_stop_has_no_worktree_scope() {
        with_app(
            "app::build_cache::tests::resume_checks_running_cleanup_before_closing_and_sandbox_stop_has_no_worktree_scope",
            |app, _| {
                let (dir, repo) = repository();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                app.runtime.sessions = vec![session("exited", AgentKind::Generic, AgentStatus::Exited, &work)];
                let (release, wait) = channel();
                app.runtime.lifecycle(Scope { sandboxes: vec![], worktrees: Some(vec![work.clone()]) }, move || {
                    let _ = wait.recv_timeout(Duration::from_secs(10));
                });
                app.runtime.resume_exited("exited");
                assert_eq!(app.runtime.sessions[0].id, "exited");
                assert!(app.runtime.resume.is_empty());
                release.send(()).unwrap();
                app.runtime.finish_lifecycle_jobs();
                let bin = crate::test_support::tempdir().unwrap();
                app.runtime.sbx = crate::sandbox::fake::install(bin.path()).to_string_lossy().into_owned();
                app.runtime.sessions[0].sandbox = Some("atomics-existing".into());
                app.runtime.schedule_cache_cleanup(&work);
                app.runtime.close_for_resume("exited");
                assert!(app.runtime.build_caches.cleanup.is_empty());
                assert!(app.runtime.lifecycle_jobs.iter().all(|job| !job.touches(None, Some(&work))));
                std::fs::create_dir(work.join("src")).unwrap();
                app.runtime.schedule_cache_cleanup(&work);
                app.runtime.cancel_cache_cleanup(&work.join("src"));
                assert!(app.runtime.build_caches.cleanup.is_empty(), "starting in a subdirectory cancels root cleanup");
            },
        );
    }

    #[test]
    fn cleanup_is_scheduled_once_and_manual_batches_discard_busy_worktrees() {
        with_app(
            "app::build_cache::tests::cleanup_is_scheduled_once_and_manual_batches_discard_busy_worktrees",
            |app, _| {
                let (dir, repo) = repository();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                app.runtime.config.cleanup.completed_plans = false;
                app.runtime.repos = vec![repo];
                app.runtime.sessions = vec![session("busy", AgentKind::Claude, AgentStatus::Working, &work)];
                app.runtime.clean_build_caches();
                assert!(app.runtime.build_caches.cleanup.is_empty());
                assert!(!app.runtime.build_caches.cleaning);
                app.runtime.sessions.clear();
                app.runtime.build_caches.cleaned.insert(work.canonicalize().unwrap());
                app.runtime.schedule_cache_cleanup(&work);
                assert!(app.runtime.build_caches.cleanup.is_empty());
                app.runtime.plans = vec![Plan {
                    id: "finished".into(),
                    tasks: vec![Task {
                        id: "task".into(),
                        status: TaskStatus::Done,
                        finished_at: Some(1),
                        worktree: Some(work.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }];
                app.runtime.build_caches.cleaned.clear();
                for _ in 0..3 {
                    app.runtime.finish_plan_agents();
                }
                assert!(app.runtime.build_caches.cleanup.is_empty(), "already finished tasks must not re-arm cleanup");
                app.runtime.plans[0].tasks[0].finished_at = None;
                app.runtime.finish_plan_agents();
                assert_eq!(app.runtime.build_caches.cleanup.len(), 1, "the finished transition schedules once");
                app.runtime.build_caches.cleanup.clear();
                app.runtime.finish_plan_agents();
                assert!(app.runtime.build_caches.cleanup.is_empty());
                app.runtime.schedule_cache_cleanup(&work);
                assert_eq!(app.runtime.build_caches.cleanup.len(), 1);
                app.runtime.cancel_cache_cleanup(&work);
                assert!(app.runtime.build_caches.cleanup.is_empty());
            },
        );
    }

    #[test]
    fn disk_guard_uses_cached_inventory_and_frequent_samples_do_not_rewalk_outputs() {
        with_app(
            "app::build_cache::tests::disk_guard_uses_cached_inventory_and_frequent_samples_do_not_rewalk_outputs",
            |app, _| {
                let (dir, repo) = repository();
                std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
                git::git(&repo, &["add", "."]).unwrap();
                git::git(&repo, &["commit", "-qm", "workspace"]).unwrap();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                std::fs::create_dir(work.join("target")).unwrap();
                std::fs::write(work.join("target/artifact"), "compiled").unwrap();
                app.runtime.repos = vec![repo.clone()];
                app.runtime.config.resources.disk_min_gib = u64::MAX;
                let cached = BuildOutput {
                    path: repo.join("cached-only").to_string_lossy().into_owned(),
                    bytes: 1024,
                    reason: None,
                };
                app.runtime.build_caches.largest = vec![cached.clone()];
                app.runtime.build_caches.inventory_at = Some(Instant::now());
                assert!(app.runtime.check_disk(&repo).unwrap_err().contains("cached-only"));
                app.runtime.build_caches.largest.clear();
                let warning = app.runtime.check_disk(&work).unwrap_err();
                assert!(!warning.contains("cached-only"));
                assert!(
                    !warning.contains(work.join("target").to_string_lossy().as_ref()),
                    "launch checks must not inventory output when the cache is empty"
                );
                app.runtime.build_caches.largest = vec![cached.clone()];
                app.runtime.poll_build_caches();
                let deadline = Instant::now() + Duration::from_secs(10);
                while app.runtime.build_caches.reading.is_some() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                    app.runtime.poll_build_caches();
                }
                assert_eq!(app.runtime.build_caches.largest, [cached]);
            },
        );
    }

    #[test]
    fn shells_chief_and_main_checkout_agents_keep_the_inherited_build_profile() {
        with_app(
            "app::build_cache::tests::shells_chief_and_main_checkout_agents_keep_the_inherited_build_profile",
            |app, _| {
                let (dir, repo) = repository();
                std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
                git::git(&repo, &["add", "Cargo.toml"]).unwrap();
                git::git(&repo, &["commit", "-qm", "workspace"]).unwrap();
                let work = dir.path().join("task");
                git::add_worktree(&repo, &work, "task", "main").unwrap();
                app.runtime.config.isolation.mode = atomics_core::isolation::IsolationMode::Bare;
                app.runtime.config.cache.env.insert("CARGO_PROFILE_DEV_DEBUG".into(), "full".into());
                for (name, kind, cwd, chief) in [
                    ("shell-profile", AgentKind::Shell, work, false),
                    ("main-profile", AgentKind::Generic, repo.clone(), false),
                    ("chief-profile", AgentKind::Claude, repo, true),
                ] {
                    let output = dir.path().join(name);
                    let command = vec![
                        "sh".into(),
                        "-c".into(),
                        format!(
                            "env > '{}.tmp'; mv '{}.tmp' '{}'; sleep 60",
                            output.display(),
                            output.display(),
                            output.display()
                        ),
                    ];
                    let id = if chief {
                        app.runtime.config.chief.command = command;
                        persist_config(app.runtime);
                        app.runtime.start_as("chief", cwd, None, vec![], None, Role::Chief).unwrap()
                    } else {
                        app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                            name: name.into(),
                            command,
                            kind,
                            busy_text: None,
                            isolation: None,
                        });
                        persist_config(app.runtime);
                        app.runtime.start(name, cwd, None, vec![], None).unwrap()
                    };
                    wait_started(app.runtime, &id);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !output.exists() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let env = std::fs::read_to_string(&output).unwrap();
                    for key in
                        ["CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG", "RUSTC_WRAPPER"]
                    {
                        let value = env.lines().find_map(|line| line.strip_prefix(&format!("{key}=")));
                        assert_eq!(value, std::env::var(key).ok().as_deref(), "{name}: {key}");
                    }
                    app.runtime.close_for_resume(&id);
                }
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
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "disk-probe".into(),
                    command: vec!["sh".into()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                let error = app.runtime.start("disk-probe", repo.clone(), None, vec![], None).unwrap_err();
                assert!(error.contains("New tasks and agent sessions are paused"));
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
