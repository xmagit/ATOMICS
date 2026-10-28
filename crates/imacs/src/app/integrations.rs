use super::*;

impl App {
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
            self.integration_checked_at = None;
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
                task.integration_verified = checked.valid;
                if !checked.valid {
                    task.integrated = None;
                    self.save_plan(index);
                }
            }
        }
        let due = self.integration_checked_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(5));
        let mut jobs = Vec::new();
        for plan in self.plans.iter().filter(|p| p.is_open()) {
            let into = plan.integration_branch();
            for task in &plan.tasks {
                if task.integrated.as_deref() != Some(&into) || (!due && task.integration_verified) {
                    continue;
                }
                if self.integration_checks.insert((plan.id.clone(), task.id.clone())) {
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
            self.integration_checked_at = Some(Instant::now());
            let (tx, ctx, revision) = (self.integration_check_tx.clone(), self.ctx.clone(), self.integration_revision);
            std::thread::spawn(move || {
                for (plan, task, into, repo) in jobs {
                    let valid = matches!(task.status, TaskStatus::Review | TaskStatus::Done)
                        && task.branch.as_deref().zip(repo).is_some_and(|(branch, repo)| {
                            git::git(
                                &repo,
                                &[
                                    "merge-base",
                                    "--is-ancestor",
                                    &format!("refs/heads/{branch}"),
                                    &format!("refs/heads/{into}"),
                                ],
                            )
                            .is_ok()
                        });
                    let _ = tx.send(CheckedIntegration { revision, plan, task, into, valid });
                    ctx.request_repaint();
                }
            });
        }
        if self.plans.iter().any(|p| p.is_open() && p.tasks.iter().any(|t| t.integrated.is_some())) {
            self.ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Commits a review or done task's work and merges its branch into the plan's
    /// integration branch, off the UI thread; the outcome lands on the task.
    pub(super) fn integrate(&mut self, index: usize, id: &str, reply: Option<Sender<ControlOutcome>>) {
        let job = match self.plans[index].tasks.iter().find(|t| t.id == id) {
            None => Err(format!("no task {id}")),
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
        let (note, response) = match result {
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
                    "plan": plan,
                    "task": id,
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
            let _ = reply.send(ControlOutcome::Done(response));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, request, with_app};
    use super::*;

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
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", name, "--nocapture"])
                    .current_dir(dir.path())
                    .env("IMACS_APP_TEST_CHILD", "1")
                    .env("HOME", dir.path())
                    .env("PATH", path)
                    .env("SHELL", "/bin/sh")
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
            let index = app.selected_plan;
            app.plans[index].approved = true;
            app.plans[index].tasks[0].status = TaskStatus::Done;
            app.plans[index].tasks[0].integrated = Some("integration".into());
            app.refresh_integrations();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !paths::home().join("started").exists() {
                assert!(Instant::now() < deadline, "background git did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(app.plans[index].ready().is_empty());
            assert_eq!(app.plan_json(index)["ready"], json!([]));
            assert!(app.dispatch(index, Some("b")).is_err(), "dispatch must agree while git is blocked");
            assert!(app.creating.is_empty());
            let checked = app.plans[index].tasks[0].clone();
            app.integration_dirty.store(true, Ordering::Relaxed);
            app.refresh_integrations();
            app.integration_check_tx
                .send(CheckedIntegration {
                    revision: app.integration_revision - 1,
                    plan: app.plans[index].id.clone(),
                    task: checked,
                    into: "integration".into(),
                    valid: true,
                })
                .unwrap();
            app.refresh_integrations();
            assert!(!app.plans[index].tasks[0].integration_verified, "a stale result must not restore readiness");
            std::fs::write(paths::home().join("release"), "").unwrap();
            while !app.plans[index].tasks[0].integration_verified {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
                app.refresh_integrations();
            }
            assert_eq!(app.plan_json(index)["ready"], json!(["b"]));
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
                let index = app.selected_plan;
                app.plans[index].approved = true;
                app.plans[index].tasks[0].status = TaskStatus::Done;
                app.plans[index].tasks[0].integrated = Some("integration".into());
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
                assert!(app.plans[index].tasks[0].integrated.is_none());
                app.finish_integration(Integrated {
                    plan: plan.as_str().unwrap().into(),
                    task: "a".into(),
                    branch: "task-a".into(),
                    into: "integration".into(),
                    reply: None,
                    result: Ok(Integration { committed: false, merge: Merge::UpToDate, worktree: repo.clone() }),
                });
                assert!(
                    app.plans[index].tasks[0].integrated.is_none(),
                    "a late integration must not mark a running task integrated"
                );
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done"})).ok);
                assert!(app.dispatch(index, Some("b")).unwrap_err().contains("integrated dependencies"));
                app.plans[index].tasks[0].integrated = Some("integration".into());
                git::git(&repo, &["switch", "task-a"]).unwrap();
                git::git(&repo, &["commit", "--allow-empty", "-qm", "new work after integration"]).unwrap();
                assert!(app.dispatch(index, Some("b")).unwrap_err().contains("integrated dependencies"));
                let deadline = Instant::now() + Duration::from_secs(5);
                while !app.integration_checks.is_empty() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                    app.refresh_integrations();
                }
                assert!(app.plans[index].tasks[0].integrated.is_none());
                assert!(app.creating.is_empty());
                let saved =
                    Plan::load(&paths::imacs_home().join("plans").join(format!("{}.json", plan.as_str().unwrap())))
                        .unwrap();
                assert!(saved.tasks[0].integrated.is_none());
                git::git(&repo, &["branch", "-f", "integration", "task-a"]).unwrap();
                app.plans[index].tasks[0].integrated = Some("integration".into());
                // A missing requested task does not invalidate a genuinely integrated dependency.
                assert!(app.dispatch(index, Some("missing")).is_err());
                assert!(app.plans[index].ready().is_empty());
                while !app.integration_checks.is_empty() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                    app.refresh_integrations();
                }
                assert_eq!(app.plans[index].tasks[0].integrated.as_deref(), Some("integration"));
                assert!(app.plans[index].ready().iter().any(|task| task.id == "b"));
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
                app.config.worktree_path = "{repo_parent}/{slug}".into();
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
                    let index = app.selected_plan;
                    app.plans[index].tasks[0].status = TaskStatus::Done;
                    indices.push(index);
                }
                app.integrate(indices[0], "a", None);
                let deadline = Instant::now() + Duration::from_secs(5);
                while !started.exists() {
                    if Instant::now() >= deadline {
                        std::fs::write(&release, "").unwrap();
                        panic!("first integration did not reach its hook");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.integrate(indices[1], "a", None);
                let other = app.integrate_rx.recv_timeout(Duration::from_secs(5));
                std::fs::write(&release, "").unwrap();
                let other = other.expect("a second repository must not wait for the first repository's integration");
                assert_eq!(other.plan, app.plans[indices[1]].id);
                assert!(other.result.is_ok());
                app.finish_integration(other);
                let first = app.integrate_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert_eq!(first.plan, app.plans[indices[0]].id);
                assert!(first.result.is_ok());
                app.finish_integration(first);
                assert!(app.plans[indices[0]].tasks[0].integrated.is_some());
                assert!(app.plans[indices[1]].tasks[0].integrated.is_some());
            },
        );
    }
}
