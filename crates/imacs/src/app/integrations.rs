use super::*;

impl State {
    /// The repository a task works in.
    pub(super) fn task_repo(&self, index: usize, task: &Task) -> Option<PathBuf> {
        task.repo
            .as_deref()
            .map(paths::expand)
            .or_else(|| {
                let session = task.session.as_ref().and_then(|id| self.sessions.iter().find(|s| &s.id == id))?;
                self.project_of(&session.root())
            })
            .or_else(|| self.plans[index].project.clone())
            .or_else(|| self.diff_target().and_then(|t| self.project_of(&t)))
            .or_else(|| self.repos.first().cloned())
    }

    pub(super) fn refresh_integrations(&mut self) {
        if self.integration_dirty.swap(false, Ordering::Relaxed) {
            self.integration_revision += 1;
            for task in self.plans.iter_mut().flat_map(|p| &mut p.tasks) {
                task.integration_verified = false;
            }
        }
        while let Ok(checked) = self.integration_check_rx.try_recv() {
            self.integration_checks.remove(&(checked.plan.clone(), checked.task.id.clone()));
            let Some(index) = self.plans.iter().position(|p| p.id == checked.plan && p.is_open()) else { continue };
            if self.integration_revision != checked.revision || self.plans[index].integration_branch() != checked.into {
                continue;
            }
            if let Some(task) = self.plans[index].task_mut(&checked.task.id)
                && *task == checked.task
            {
                task.integration_verified = checked.valid == Some(true);
                if checked.valid == Some(false) {
                    task.integrated = None;
                }
                self.integration_checked
                    .insert((checked.plan, task.id.clone()), (checked.revision, checked.into, task.clone()));
                if checked.valid == Some(false) {
                    self.save_plan(index);
                }
            }
        }
        let mut jobs = Vec::new();
        for plan in self.plans.iter_mut().filter(|p| p.is_open()) {
            let into = plan.integration_branch();
            for task in &mut plan.tasks {
                let key = (plan.id.clone(), task.id.clone());
                if task.integrated.as_deref() != Some(&into)
                    || self.integration_checked.get(&key).is_some_and(|(revision, branch, checked)| {
                        *revision == self.integration_revision && branch == &into && checked == task
                    })
                {
                    continue;
                }
                if self.integration_checks.insert(key.clone()) {
                    task.integration_verified = false;
                    // Remember errors too: retry only when refs or the task change.
                    self.integration_checked.insert(key, (self.integration_revision, into.clone(), task.clone()));
                    let repo = task
                        .repo
                        .as_deref()
                        .map(paths::expand)
                        .or_else(|| plan.project.clone())
                        .or_else(|| {
                            self.sessions.iter().find(|s| task.session.as_deref() == Some(&s.id)).map(Session::root)
                        })
                        .or_else(|| self.repos.first().cloned());
                    jobs.push((plan.id.clone(), task.clone(), into.clone(), repo));
                }
            }
        }
        if !jobs.is_empty() {
            let (tx, ctx, revision) = (self.integration_check_tx.clone(), self.ctx.clone(), self.integration_revision);
            std::thread::spawn(move || {
                for (plan, task, into, repo) in jobs {
                    let valid = task
                        .branch
                        .as_deref()
                        .zip(repo)
                        .filter(|_| matches!(task.status, TaskStatus::Review | TaskStatus::Done))
                        .and_then(|(branch, repo)| {
                            let output = std::process::Command::new(resolve_program("git"))
                                .arg("-C")
                                .arg(repo)
                                .args(["-c", "core.quotepath=off"])
                                .args([
                                    "merge-base",
                                    "--is-ancestor",
                                    &format!("refs/heads/{branch}"),
                                    &format!("refs/heads/{into}"),
                                ])
                                .output()
                                .ok()?;
                            match output.status.code() {
                                Some(0) => Some(true),
                                Some(1) => Some(false),
                                _ => None,
                            }
                        });
                    let _ = tx.send(CheckedIntegration { revision, plan, task, into, valid });
                    ctx.request_repaint();
                }
            });
        }
    }

    /// Commits a review or done task's work and merges its branch into the plan's
    /// integration branch, off the UI thread; the outcome lands on the task.
    pub(super) fn integrate(&mut self, index: usize, id: &str, reply: Option<Sender<ControlOutcome>>) {
        let job = match self.plans[index].tasks.iter().find(|t| t.id == id) {
            None => Err(format!("no task {id}")),
            Some(_) if id == plan::REVIEW_TASK => {
                Err("The review task cannot be integrated: it only reviews the plan's integration branch.".into())
            }
            Some(task) if !matches!(task.status, TaskStatus::Review | TaskStatus::Done) => {
                Err(format!("Task {id} is {}: only tasks in review or done can be integrated.", task.status.label()))
            }
            Some(_) if self.integrating.contains(&(self.plans[index].id.clone(), id.to_string())) => {
                Err(format!("Task {id} is already being integrated."))
            }
            Some(task) => match (task.branch.clone(), self.task_repo(index, task)) {
                (None, _) => Err(format!("Task {id} has no branch yet: dispatch it first.")),
                (_, None) => Err("No project is open to integrate into; give the task a repo.".into()),
                (Some(branch), Some(repo)) => Ok((repo, branch, format!("{} ({})", task.title.trim(), task.id))),
            },
        };
        let (repo, branch, message) = match job {
            Ok(job) => job,
            Err(err) => {
                match reply {
                    Some(reply) => {
                        let _ = reply.send(ControlOutcome::Done(ControlResponse::error(err)));
                    }
                    None => self.toast(err),
                }
                return;
            }
        };
        let into = self.plans[index].integration_branch();
        let into_path = git::worktree_path(&self.config.worktree_path, &repo, &into);
        let (tx, base_ref, ctx) = (self.integrate_tx.clone(), self.config.base_ref.clone(), self.ctx.clone());
        let task = id.to_string();
        let plan = self.plans[index].id.clone();
        self.integrating.insert((plan.clone(), task.clone()));
        std::thread::spawn(move || {
            let result =
                git::integrate(&repo, &branch, &message, &into, &into_path, &base_ref).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Integrated { plan, task, branch, into, result, reply });
            ctx.request_repaint();
        });
    }

    pub(super) fn finish_integration(&mut self, done: Integrated) {
        let Integrated { plan, task: id, branch, into, result, reply } = done;
        let Some(index) = self.plans.iter().position(|p| p.id == plan) else { return };
        self.integrating.remove(&(plan.clone(), id.clone()));
        self.worktrees_at = None;
        let (note, mut response) = match result {
            Ok(Integration { merge: Merge::Conflict(files), .. }) => {
                if let Some(task) = self.plans[index].task_mut(&id) {
                    task.set_status(TaskStatus::Review);
                }
                let files = files.join(", ");
                let error = format!(
                    "Merging {branch} into {into} conflicts in {files}. The merge was aborted; task {id} stays in review."
                );
                (format!("conflicts with {into} in {files}; merge aborted"), ControlResponse::error(error))
            }
            Ok(Integration { committed, merge, worktree }) => {
                if let Some(task) = self.plans[index].task_mut(&id)
                    && matches!(task.status, TaskStatus::Review | TaskStatus::Done)
                {
                    task.integrated = Some(into.clone());
                    task.integration_verified = true;
                }
                let merged = merge == Merge::Merged;
                let note = match (committed, merged) {
                    (true, _) => format!("committed and merged into {into}"),
                    (false, true) => format!("merged into {into}"),
                    (false, false) => format!("already in {into}"),
                };
                let result = json!({
                    "integration": into,
                    "worktree": worktree,
                    "committed": committed,
                    "merged": merged,
                });
                (note, ControlResponse::ok(result))
            }
            Err(err) => (format!("could not integrate: {err}"), ControlResponse::error(err)),
        };
        if let Some(task) = self.plans[index].task_mut(&id) {
            task.notes.push(note.clone());
            self.save_plan(index);
        }
        self.finish_plan_agents();
        self.toast(format!("Task {id}: {note}"));
        if let Some(reply) = reply {
            let mut summary = self.plan_mutation_json(index, Some(&id));
            if let Some(result) = response.result.as_object() {
                summary.as_object_mut().unwrap().extend(result.clone());
            }
            response.result = summary;
            let _ = reply.send(ControlOutcome::Done(response));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, request, with_app};
    use super::*;

    #[test]
    fn review_task_cannot_be_integrated() {
        with_app("app::integrations::tests::review_task_cannot_be_integrated", |app, _| {
            let (_dir, repo) = repository();
            request(app, json!({"cmd":"plan_set", "goal":"Review integration", "tasks":[{"id":"a", "repo":repo}]}));
            let index = app.runtime.selected_plan;
            let plan = app.runtime.plans[index].id.clone();
            let into = app.runtime.plans[index].integration_branch();
            git::git(&repo, &["branch", &into]).unwrap();
            let review = repo.with_file_name("review");
            git::add_worktree(&repo, &review, "review-branch", "main").unwrap();
            std::fs::write(review.join("uncommitted"), "keep review changes").unwrap();
            let before = git::git(&repo, &["rev-parse", &into, "review-branch"]).unwrap();
            for status in [TaskStatus::Planned, TaskStatus::Review, TaskStatus::Done] {
                let task = app.runtime.plans[index].task_mut(plan::REVIEW_TASK).unwrap();
                task.status = status;
                task.branch = Some("review-branch".into());
                let response = request(app, json!({"cmd":"plan_integrate", "plan":plan, "id":plan::REVIEW_TASK}));
                assert!(!response.ok);
                assert!(response.error.unwrap().contains("The review task cannot be integrated"));
                assert!(app.runtime.integrating.is_empty());
                assert!(app.runtime.integrate_rx.try_recv().is_err());
                assert!(app.runtime.plans[index].task_mut(plan::REVIEW_TASK).unwrap().integrated.is_none());
            }
            assert_eq!(git::git(&repo, &["rev-parse", &into, "review-branch"]).unwrap(), before);
            assert_eq!(git::git(&review, &["status", "--porcelain"]).unwrap().trim(), "?? uncommitted");
        });
    }

    #[test]
    fn integrated_tasks_in_other_repositories_are_rechecked_after_commits() {
        with_app(
            "app::integrations::tests::integrated_tasks_in_other_repositories_are_rechecked_after_commits",
            |app, ctx| {
                let (_project_dir, project) = repository();
                let (_task_dir, repo) = repository();
                git::git(&repo, &["branch", "task-a"]).unwrap();
                git::git(&repo, &["branch", "integration"]).unwrap();
                request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Cross repository freshness",
                "integration":"integration", "tasks":[{"id":"a", "repo":repo, "branch":"task-a"},
                {"id":"b", "depends_on":["a"]}]}),
                );
                let index = app.runtime.selected_plan;
                app.runtime.plans[index].project = Some(project);
                app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
                app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
                app.runtime.sessions.clear();
                app.runtime.active = None;
                app.runtime.diff_root = None;
                app.runtime.runtime_tick(ctx);
                assert!(
                    app.runtime.watched.lock().unwrap().iter().any(|(root, _)| root == &repo),
                    "the task repository must be watched even without a live agent or visible diff"
                );
                let deadline = Instant::now() + Duration::from_secs(10);
                while !app.runtime.plans[index].tasks[0].integration_verified {
                    assert!(Instant::now() < deadline, "initial integration check did not finish");
                    std::thread::sleep(Duration::from_millis(10));
                    app.runtime.runtime_tick(ctx);
                }
                assert_eq!(app.runtime.plans[index].ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["b"]);
                git::git(&repo, &["switch", "task-a"]).unwrap();
                git::git(&repo, &["commit", "--allow-empty", "-qm", "later task work"]).unwrap();
                while app.runtime.plans[index].tasks[0].integrated.is_some() {
                    assert!(Instant::now() < deadline, "a later commit in the task repository was not detected");
                    std::thread::sleep(Duration::from_millis(25));
                    app.runtime.runtime_tick(ctx);
                }
                assert!(!app.runtime.plans[index].tasks[0].integration_verified);
                assert!(app.runtime.plans[index].ready().is_empty());
            },
        );
    }

    #[test]
    fn idle_integrations_do_not_poll_git_or_request_repaints() {
        with_app("app::integrations::tests::idle_integrations_do_not_poll_git_or_request_repaints", |app, _| {
            let (_dir, repo) = repository();
            git::git(&repo, &["branch", "task-a"]).unwrap();
            git::git(&repo, &["branch", "integration"]).unwrap();
            request(
                app,
                json!({"cmd":"plan_set", "goal":"Idle integration", "integration":"integration", "tasks":[
                    {"id":"a", "repo":repo, "branch":"task-a"}]}),
            );
            let index = app.runtime.selected_plan;
            app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
            app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
            // Isolate the integration refresh from other app repaint sources.
            app.runtime.ctx = egui::Context::default();
            app.runtime.refresh_integrations();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !app.runtime.plans[index].tasks[0].integration_verified {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
                app.runtime.refresh_integrations();
            }
            let ctx = app.runtime.ctx.clone();
            for _ in 0..3 {
                ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
            }
            std::thread::sleep(Duration::from_millis(5100));
            let output = ctx.run_ui(Default::default(), |_| app.runtime.refresh_integrations());
            assert!(app.runtime.integration_checks.is_empty(), "idle refresh must not start another git check");
            assert_eq!(output.viewport_output[&egui::ViewportId::ROOT].repaint_delay, Duration::MAX);
            output.drop_without_applying_deltas();
            assert!(app.runtime.integration_check_rx.try_recv().is_err(), "idle refresh must not run git");
        });
    }

    #[test]
    fn integration_git_errors_preserve_markers_until_refs_change() {
        with_app("app::integrations::tests::integration_git_errors_preserve_markers_until_refs_change", |app, _| {
            let (_dir, repo) = repository();
            git::git(&repo, &["branch", "integration"]).unwrap();
            request(
                app,
                json!({"cmd":"plan_set", "goal":"Unavailable ref", "integration":"integration", "tasks":[
                    {"id":"a", "repo":repo, "branch":"task-a"}, {"id":"b", "depends_on":["a"]}]}),
            );
            let index = app.runtime.selected_plan;
            app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
            app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
            app.runtime.save_plan(index);
            // Loading a persisted marker must trigger a check, even without a ref event.
            let path = paths::imacs_home().join("plans").join(format!("{}.json", app.runtime.plans[index].id));
            app.runtime.plans[index] = Plan::load(&path).unwrap();
            app.runtime.refresh_integrations();
            assert!(!app.runtime.integration_checks.is_empty());
            let deadline = Instant::now() + Duration::from_secs(5);
            while !app.runtime.integration_checks.is_empty() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
                app.runtime.refresh_integrations();
            }
            assert_eq!(app.runtime.plans[index].tasks[0].integrated.as_deref(), Some("integration"));
            assert!(!app.runtime.plans[index].tasks[0].integration_verified);
            assert!(app.runtime.plans[index].ready().is_empty());
            assert_eq!(Plan::load(&path).unwrap().tasks[0].integrated.as_deref(), Some("integration"));

            git::git(&repo, &["branch", "task-a"]).unwrap();
            app.runtime.refresh_integrations();
            assert!(
                app.runtime.integration_checks.is_empty(),
                "a git error must wait for a ref change before retrying"
            );
            assert!(!app.runtime.plans[index].tasks[0].integration_verified);
            app.runtime.integration_dirty.store(true, Ordering::Relaxed);
            app.runtime.refresh_integrations();
            while !app.runtime.integration_checks.is_empty() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
                app.runtime.refresh_integrations();
            }
            assert!(app.runtime.plans[index].tasks[0].integration_verified);
            assert_eq!(app.runtime.plans[index].ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["b"]);
        });
    }

    #[test]
    fn integration_checks_run_in_background_and_ignore_invalidated_results() {
        let name = "app::integrations::tests::integration_checks_run_in_background_and_ignore_invalidated_results";
        if std::env::var_os("IMACS_APP_TEST_CHILD").is_none() {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let real_git = crate::setup::resolve_program("git");
            let real_git = format!("'{}'", real_git.replace('\'', "'\\''"));
            let wrapper = dir.path().join("git");
            std::fs::write(
                &wrapper,
                format!(
                    r#"#!/bin/sh
if [ "$5" = merge-base ]; then
  : > "$HOME/started"
  attempts=0
  while [ ! -e "$HOME/release" ]; do
    attempts=$((attempts + 1))
    [ "$attempts" -lt 1000 ] || exit 1
    /bin/sleep 0.01
  done
fi
exec {} "$@"
"#,
                    real_git
                ),
            )
            .unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap());
            let shell = dir.path().join("shell");
            std::fs::write(&shell, "#!/bin/sh\nprintf '__IMACS__%s__IMACS__' \"$IMACS_TEST_LOGIN_PATH\"\n").unwrap();
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", name, "--nocapture"])
                    .current_dir(dir.path())
                    .env("IMACS_APP_TEST_CHILD", "1")
                    .env("HOME", dir.path())
                    // The bundled app must find the wrapper through the login shell,
                    // even when its process PATH only exposes the system git.
                    .env("PATH", "/usr/bin:/bin")
                    .env("IMACS_TEST_LOGIN_PATH", path)
                    .env("SHELL", shell)
                    .env("IMACS_HOME", dir.path().join("state"))
                    .env("IMACS_CONFIG", dir.path().join("config.toml"))
                    .env("IMACS_SOCKET", dir.path().join("imacs.sock")),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        with_app(name, |app, _| {
            let (_dir, repo) = repository();
            git::git(&repo, &["branch", "task-a"]).unwrap();
            git::git(&repo, &["branch", "integration"]).unwrap();
            request(
                app,
                json!({"cmd":"plan_set", "goal":"Background ancestry", "integration":"integration", "tasks":[
                {"id":"a", "repo":repo, "branch":"task-a"}, {"id":"b", "repo":repo, "depends_on":["a"]}]}),
            );
            let index = app.runtime.selected_plan;
            app.runtime.plans[index].approved = true;
            app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
            app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
            app.runtime.refresh_integrations();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !paths::home().join("started").exists() {
                assert!(Instant::now() < deadline, "background git did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(app.runtime.plans[index].ready().is_empty());
            assert_eq!(app.runtime.plan_json(index)["ready"], json!([]));
            assert!(app.runtime.dispatch(index, Some("b")).is_err(), "dispatch must agree while git is blocked");
            assert!(app.runtime.creating.is_empty());
            let checked = app.runtime.plans[index].tasks[0].clone();
            app.runtime.integration_dirty.store(true, Ordering::Relaxed);
            app.runtime.refresh_integrations();
            app.runtime
                .integration_check_tx
                .send(CheckedIntegration {
                    revision: app.runtime.integration_revision - 1,
                    plan: app.runtime.plans[index].id.clone(),
                    task: checked,
                    into: "integration".into(),
                    valid: Some(true),
                })
                .unwrap();
            app.runtime.refresh_integrations();
            assert!(
                !app.runtime.plans[index].tasks[0].integration_verified,
                "a stale result must not restore readiness"
            );
            std::fs::write(paths::home().join("release"), "").unwrap();
            while !app.runtime.plans[index].tasks[0].integration_verified {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
                app.runtime.refresh_integrations();
            }
            assert_eq!(app.runtime.plan_json(index)["ready"], json!(["b"]));
        });
    }

    #[test]
    fn status_updates_clear_integration_and_dispatch_rejects_new_unmerged_commits() {
        with_app(
            "app::integrations::tests::status_updates_clear_integration_and_dispatch_rejects_new_unmerged_commits",
            |app, _| {
                let (_dir, repo) = repository();
                git::git(&repo, &["branch", "task-a"]).unwrap();
                git::git(&repo, &["branch", "integration"]).unwrap();
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Freshness", "integration":"integration", "tasks":[
                {"id":"a", "repo":repo, "branch":"task-a"}, {"id":"b", "repo":repo, "depends_on":["a"]}]}),
                )
                .result["plan"]
                    .clone();
                let index = app.runtime.selected_plan;
                app.runtime.plans[index].approved = true;
                app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
                app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
                assert!(app.runtime.plans[index].tasks[0].integrated.is_none());
                app.runtime.finish_integration(Integrated {
                    plan: plan.as_str().unwrap().into(),
                    task: "a".into(),
                    branch: "task-a".into(),
                    into: "integration".into(),
                    reply: None,
                    result: Ok(Integration { committed: false, merge: Merge::UpToDate, worktree: repo.clone() }),
                });
                assert!(
                    app.runtime.plans[index].tasks[0].integrated.is_none(),
                    "a late integration must not mark a running task integrated"
                );
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done"})).ok);
                assert!(app.runtime.dispatch(index, Some("b")).unwrap_err().contains("integrated dependencies"));
                app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
                git::git(&repo, &["switch", "task-a"]).unwrap();
                git::git(&repo, &["commit", "--allow-empty", "-qm", "new work after integration"]).unwrap();
                assert!(app.runtime.dispatch(index, Some("b")).unwrap_err().contains("integrated dependencies"));
                let deadline = Instant::now() + Duration::from_secs(5);
                while !app.runtime.integration_checks.is_empty() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                    app.runtime.refresh_integrations();
                }
                assert!(app.runtime.plans[index].tasks[0].integrated.is_none());
                assert!(app.runtime.creating.is_empty());
                let saved =
                    Plan::load(&paths::imacs_home().join("plans").join(format!("{}.json", plan.as_str().unwrap())))
                        .unwrap();
                assert!(saved.tasks[0].integrated.is_none());
                git::git(&repo, &["branch", "-f", "integration", "task-a"]).unwrap();
                app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
                // A missing requested task does not invalidate a genuinely integrated dependency.
                assert!(app.runtime.dispatch(index, Some("missing")).is_err());
                assert!(app.runtime.plans[index].ready().is_empty());
                while !app.runtime.integration_checks.is_empty() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                    app.runtime.refresh_integrations();
                }
                assert_eq!(app.runtime.plans[index].tasks[0].integrated.as_deref(), Some("integration"));
                assert!(app.runtime.plans[index].ready().iter().any(|task| task.id == "b"));
            },
        );
    }

    #[test]
    fn integrations_in_different_repositories_can_finish_independently() {
        use std::os::unix::fs::PermissionsExt;
        with_app(
            "app::integrations::tests::integrations_in_different_repositories_can_finish_independently",
            |app, _| {
                let (dir1, repo1) = repository();
                let (_dir2, repo2) = repository();
                let started = dir1.path().join("started");
                let release = dir1.path().join("release");
                let hook = repo1.join(".git/hooks/pre-commit");
                std::fs::write(
                    &hook,
                    format!(
                        "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.02; done\n",
                        started.display(),
                        release.display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
                app.runtime.config.worktree_path = "{repo_parent}/{slug}".into();
                let mut indices = Vec::new();
                for (goal, repo) in [("One", &repo1), ("Two", &repo2)] {
                    git::git(repo, &["branch", "task"]).unwrap();
                    let worktree = repo.with_file_name("task-worktree");
                    git::git(repo, &["worktree", "add", worktree.to_str().unwrap(), "task"]).unwrap();
                    std::fs::write(worktree.join("work.txt"), goal).unwrap();
                    request(
                        app,
                        json!({"cmd":"plan_set", "goal":goal, "tasks":[{"id":"a", "repo":repo, "branch":"task"}]}),
                    );
                    let index = app.runtime.selected_plan;
                    app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
                    indices.push(index);
                }
                app.runtime.integrate(indices[0], "a", None);
                let deadline = Instant::now() + Duration::from_secs(5);
                while !started.exists() {
                    if Instant::now() >= deadline {
                        std::fs::write(&release, "").unwrap();
                        panic!("first integration did not reach its hook");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.integrate(indices[1], "a", None);
                let other = app.runtime.integrate_rx.recv_timeout(Duration::from_secs(5));
                std::fs::write(&release, "").unwrap();
                let other = other.expect("a second repository must not wait for the first repository's integration");
                assert_eq!(other.plan, app.runtime.plans[indices[1]].id);
                assert!(other.result.is_ok());
                app.runtime.finish_integration(other);
                let first = app.runtime.integrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(first.plan, app.runtime.plans[indices[0]].id);
                assert!(first.result.is_ok());
                app.runtime.finish_integration(first);
                assert!(app.runtime.plans[indices[0]].tasks[0].integrated.is_some());
                assert!(app.runtime.plans[indices[1]].tasks[0].integrated.is_some());
            },
        );
    }
}
