use super::*;

/// How long a readiness check git could not finish waits before it runs again.
const INTEGRATION_RETRY: Duration = Duration::from_secs(30);

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
        self.integration_retries.retain(|(plan, task), _| {
            self.plans.iter().any(|p| p.id == *plan && p.is_open() && p.tasks.iter().any(|t| t.id == *task))
        });
        if self.integration_dirty.swap(false, Ordering::Relaxed) {
            self.integration_revision += 1;
            for task in self.plans.iter_mut().flat_map(|p| &mut p.tasks) {
                task.integration_verified = false;
            }
        }
        while let Ok(checked) = self.integration_check_rx.try_recv() {
            let key = (checked.plan.clone(), checked.task.id.clone());
            self.integration_checks.remove(&key);
            let Some(index) = self.plans.iter().position(|p| p.id == checked.plan && p.is_open()) else { continue };
            if self.integration_revision != checked.revision || self.plans[index].integration_branch() != checked.into {
                continue;
            }
            if let Some(task) = self.plans[index].task_mut(&checked.task.id)
                && same_work(task, &checked.task)
            {
                task.integration_verified = checked.valid == Some(true);
                if checked.valid == Some(true) {
                    task.integrated = Some(checked.into.clone());
                    task.integrated_commit = checked.commit;
                }
                if checked.valid == Some(false) {
                    task.integrated = None;
                    task.integrated_commit = None;
                }
                if checked.retry {
                    self.integration_retries.insert(key, Instant::now() + INTEGRATION_RETRY);
                }
                self.integration_checked
                    .insert((checked.plan, task.id.clone()), (checked.revision, checked.into, task.clone()));
                if checked.valid.is_some() {
                    self.save_plan(index);
                }
            }
        }
        let (mut jobs, now) = (Vec::new(), Instant::now());
        for index in 0..self.plans.len() {
            if !self.plans[index].is_open() {
                continue;
            }
            let (plan, into) = (self.plans[index].id.clone(), self.plans[index].integration_branch());
            for position in 0..self.plans[index].tasks.len() {
                let task = &self.plans[index].tasks[position];
                let key = (plan.clone(), task.id.clone());
                let retry = self.integration_retries.get(&key).is_some_and(|at| *at <= now);
                if task.id == plan::REVIEW_TASK
                    || !(task.integrated.as_deref() == Some(&into)
                        || task.status == TaskStatus::Done && task.branch.is_some())
                    || !retry
                        && self.integration_checked.get(&key).is_some_and(|(revision, branch, checked)| {
                            *revision == self.integration_revision && branch == &into && same_work(checked, task)
                        })
                {
                    continue;
                }
                if self.integration_checks.insert(key.clone()) {
                    // The repository the task integrates into, never a sandboxed session's folder.
                    let repo = self.task_repo(index, task);
                    self.integration_retries.remove(&key);
                    let task = &mut self.plans[index].tasks[position];
                    task.integration_verified = false;
                    // Remember what git answers, errors too: check again only when refs or the task
                    // change, or after a while when git did not finish.
                    self.integration_checked.insert(key, (self.integration_revision, into.clone(), task.clone()));
                    jobs.push((plan.clone(), task.clone(), into.clone(), repo));
                }
            }
        }
        if !jobs.is_empty() {
            let jobs: Vec<_> = jobs
                .into_iter()
                .map(|(plan, task, into, repo)| {
                    let base = repo.as_deref().map(|repo| self.project_config(repo).base_ref);
                    (plan, task, into, repo, base)
                })
                .collect();
            let (tx, revision) = (self.integration_check_tx.clone(), self.integration_revision);
            std::thread::spawn(move || {
                for (plan, task, into, repo, base) in jobs {
                    let mut commit = None;
                    let checked = match (task.branch.as_deref(), repo) {
                        (Some(branch), Some(repo)) if matches!(task.status, TaskStatus::Review | TaskStatus::Done) => {
                            work_in(
                                &repo,
                                branch,
                                &into,
                                base.as_deref(),
                                task.integrated_commit.as_deref(),
                                &mut commit,
                            )
                        }
                        _ => Ok(None),
                    };
                    let (valid, retry) = match checked {
                        Ok(valid) => (valid, false),
                        Err(err) => {
                            eprintln!("IMACS: checking whether task {} is in {into} failed: {err:#}", task.id);
                            (None, true)
                        }
                    };
                    let _ = tx.send(CheckedIntegration { commit, revision, plan, task, into, valid, retry });
                }
            });
        }
    }

    /// Commits a review or done task's work and merges its branch into the plan's
    /// integration branch, off the UI thread; the outcome lands on the task.
    pub(super) fn integrate(&mut self, index: usize, id: &str, reply: Option<Sender<ControlOutcome>>) {
        let job = match self.plans[index].tasks.iter().find(|t| t.id == id) {
            _ if self.plans[index].status != PlanStatus::Open => {
                Err("Reopen the plan before integrating tasks.".into())
            }
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
        if let Err(err) = self.pin_ticket_integration(index) {
            match reply {
                Some(reply) => {
                    let _ = reply.send(ControlOutcome::Done(ControlResponse::error(err)));
                }
                None => self.toast(err),
            }
            return;
        }
        let into = self.plans[index].integration_branch();
        let into_path = git::worktree_path(&self.config.worktree_path, &repo, &into);
        let (tx, base_ref) = (self.integrate_tx.clone(), self.project_config(&repo).base_ref);
        let task = id.to_string();
        let plan = self.plans[index].id.clone();
        self.integrating.insert((plan.clone(), task.clone()));
        std::thread::spawn(move || {
            // It commits on the task branch and creates or merges into the integration branch.
            let write = crate::ref_guard::host_write(&repo, [&branch, &into]);
            let result =
                git::integrate(&repo, &branch, &message, &into, &into_path, &base_ref).map_err(|e| format!("{e:#}"));
            let commit = result
                .as_ref()
                .ok()
                .and_then(|_| git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]).ok())
                .map(|tip| tip.trim().to_string());
            drop(write);
            let _ = tx.send(Integrated { commit, plan, task, branch, into, result, reply });
        });
    }

    pub(super) fn finish_integration(&mut self, done: Integrated) {
        let Integrated { commit, plan, task: id, branch, into, result, reply } = done;
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
                    task.integrated_commit = commit;
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
        if response.ok {
            let _ = self.record_stage(
                index,
                Some(&id),
                imacs_core::workflow::Stage { stage: "integrated".into(), ..Default::default() },
                imacs_core::tickets::Author::Imacs,
            );
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

/// Whether a task's work is in `into`, or in `base` while `into` does not exist yet: the commit
/// `branch` is at, or the `saved` one once the branch is deleted, with nothing uncommitted in the
/// branch's worktree. `None` when git answers neither, an error when git did not run or finish.
/// `commit` gets the commit it checked.
fn work_in(
    repo: &Path,
    branch: &str,
    into: &str,
    base: Option<&str>,
    saved: Option<&str>,
    commit: &mut Option<String>,
) -> anyhow::Result<Option<bool>> {
    let branch_ref = format!("refs/heads/{branch}");
    let source = match git::commit_of(repo, &branch_ref)? {
        Some(tip) => tip,
        None => {
            // Only a genuinely deleted branch may use the saved tip.
            let refs = git::git(repo, &["for-each-ref", "--format=%(refname)", &branch_ref])?;
            match saved {
                Some(saved) if !refs.lines().any(|name| name == branch_ref) => saved.to_string(),
                _ => return Ok(None),
            }
        }
    };
    *commit = Some(source.clone());
    let target = format!("refs/heads/{into}");
    let target_exists = git::commit_of(repo, &target)?.is_some();
    let Some(target) = (if target_exists { Some(target.as_str()) } else { base }) else { return Ok(None) };
    Ok(match git::is_ancestor(repo, &source, target)? {
        Some(true) => {
            // A branch with no new commits can still have uncommitted work. Host git never runs in a
            // worktree a sandbox may have moved, so its work cannot count as checked.
            let trees = git::list_worktrees(repo)?;
            match trees.iter().find(|tree| tree.branch.as_deref() == Some(branch)) {
                Some(tree) if !tree.trusted => None,
                Some(tree) => {
                    Some(git::git(&tree.path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty())
                }
                None => Some(true),
            }
        }
        Some(false) if target_exists => Some(false),
        _ => None,
    })
}

/// Ticket and lifecycle metadata say nothing about a task's branch ancestry.
fn same_work(a: &Task, b: &Task) -> bool {
    let strip = |task: &Task| Task { ticket: None, stages: Vec::new(), ..task.clone() };
    strip(a) == strip(b)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, request, with_app};
    use super::*;

    #[test]
    fn retries_are_pruned_for_closed_plans_and_removed_tasks() {
        with_app("app::integrations::tests::retries_are_pruned_for_closed_plans_and_removed_tasks", |app, _| {
            let state = &mut app.runtime;
            state.plans = vec![Plan {
                id: "open".into(),
                tasks: vec![Task { id: "keep".into(), ..Default::default() }],
                ..Default::default()
            }];
            for (plan, task) in [("open", "keep"), ("open", "removed"), ("closed", "keep")] {
                state.integration_retries.insert((plan.into(), task.into()), Instant::now());
            }
            state.refresh_integrations();
            assert_eq!(state.integration_retries.len(), 1);
            assert!(state.integration_retries.contains_key(&("open".into(), "keep".into())));
            state.plans[0].status = PlanStatus::Completed;
            state.refresh_integrations();
            assert!(state.integration_retries.is_empty());
        });
    }

    #[test]
    fn deleting_an_integrated_branch_keeps_dependents_verified_after_reload() {
        with_app(
            "app::integrations::tests::deleting_an_integrated_branch_keeps_dependents_verified_after_reload",
            |app, _| {
                let (_dir, repo) = repository();
                git::git(&repo, &["branch", "task-a"]).unwrap();
                git::git(&repo, &["branch", "integration"]).unwrap();
                request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Keep evidence", "integration":"integration", "tasks":[
                {"id":"a", "repo":repo, "branch":"task-a"}, {"id":"b", "repo":repo, "depends_on":["a"]}]}),
                );
                let index = app.runtime.selected_plan;
                app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                let verify = |app: &mut Desktop<'_>| {
                    app.runtime.refresh_integrations();
                    while !app.runtime.integration_checks.is_empty() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                        app.runtime.refresh_integrations();
                    }
                };
                verify(app);
                let tip = app.runtime.plans[index].tasks[0].integrated_commit.clone().unwrap();
                assert!(!tip.trim().is_empty());
                git::git(&repo, &["branch", "-D", "task-a"]).unwrap();
                let path = paths::imacs_home().join("plans").join(format!("{}.json", app.runtime.plans[index].id));
                app.runtime.plans[index] = Plan::load(&path).unwrap();
                assert!(!app.runtime.plans[index].tasks[0].integration_verified);
                app.runtime.integration_dirty.store(true, Ordering::Relaxed);
                verify(app);
                assert!(app.runtime.plans[index].tasks[0].integration_verified);
                assert_eq!(app.runtime.plans[index].ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["b"]);
                // A recreated source branch must be checked from its current tip, not saved evidence.
                git::git(&repo, &["switch", "-c", "task-a"]).unwrap();
                git::git(&repo, &["commit", "--allow-empty", "-qm", "new work"]).unwrap();
                app.runtime.integration_dirty.store(true, Ordering::Relaxed);
                verify(app);
                assert!(!app.runtime.plans[index].tasks[0].integration_verified);
                assert!(app.runtime.plans[index].tasks[0].integrated_commit.is_none());
                assert!(app.runtime.plans[index].ready().is_empty());
            },
        );
    }

    #[test]
    fn accepted_empty_branches_complete_but_real_changes_wait_for_integration() {
        with_app(
            "app::integrations::tests::accepted_empty_branches_complete_but_real_changes_wait_for_integration",
            |app, _| {
                let (_dir, repo) = repository();
                app.runtime.config.cleanup.completed_plans = false;
                app.runtime.config.base_ref = "main".into();
                git::git(&repo, &["branch", "empty"]).unwrap();
                git::git(&repo, &["branch", "integration"]).unwrap();
                for into in ["integration", "not-created-yet"] {
                    let created = request(
                        app,
                        json!({"cmd":"plan_set", "goal":format!("Empty {into}"), "integration":into,
                    "tasks":[{"id":"a", "repo":repo, "branch":"empty"}]}),
                    );
                    let index = app.runtime.plans.iter().position(|p| p.id == created.result["plan"]).unwrap();
                    for task in &mut app.runtime.plans[index].tasks {
                        task.status = TaskStatus::Done;
                    }
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while app.runtime.plans[index].is_open() {
                        assert!(Instant::now() < deadline, "empty branch and review should finish");
                        app.runtime.refresh_integrations();
                        app.runtime.finish_plan_agents();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert!(app.runtime.plans[index].pending_integrations().is_empty());
                }
                let work = repo.with_file_name("real");
                git::add_worktree(&repo, &work, "real", "main").unwrap();
                std::fs::write(work.join("feature"), "new work").unwrap();
                git::git(&work, &["add", "."]).unwrap();
                git::git(&work, &["commit", "-qm", "real change"]).unwrap();
                let created = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Real changes", "integration":"integration",
                "tasks":[{"id":"a", "repo":repo, "branch":"real"}]}),
                );
                let index = app.runtime.plans.iter().position(|p| p.id == created.result["plan"]).unwrap();
                for task in &mut app.runtime.plans[index].tasks {
                    task.status = TaskStatus::Done;
                }
                app.runtime.refresh_integrations();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.integration_checks.is_empty() {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                    app.runtime.refresh_integrations();
                }
                app.runtime.finish_plan_agents();
                assert!(app.runtime.plans[index].is_open());
                assert_eq!(
                    app.runtime.plans[index].pending_integrations().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
                    ["a"]
                );
                assert!(app.runtime.integrating.is_empty(), "real work must never auto-integrate");
            },
        );
    }

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
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
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
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
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
            app.runtime.drain_actions();
            let index = app.runtime.selected_plan;
            app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
            app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
            app.runtime.save_plan(index);
            // Loading a persisted marker must trigger a check, even without a ref event.
            let path = paths::imacs_home().join("plans").join(format!("{}.json", app.runtime.plans[index].id));
            app.runtime.plans[index] = Plan::load(&path).unwrap();
            app.runtime.refresh_integrations();
            assert!(!app.runtime.integration_checks.is_empty());
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
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
            let dir = crate::test_support::tempdir().unwrap();
            let real_git = crate::setup::resolve_program("git");
            let real_git = format!("'{}'", real_git.replace('\'', "'\\''"));
            let wrapper = dir.path().join("git");
            std::fs::write(
                &wrapper,
                format!(
                    r#"#!/bin/sh
if [ "$5" = merge-base ]; then
  printf %s "$GIT_TERMINAL_PROMPT" > "$HOME/started.tmp" && mv "$HOME/started.tmp" "$HOME/started"
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
                    // The check runs git the way IMACS runs all repository git, from its own PATH.
                    .env("PATH", path)
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
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while !paths::home().join("started").exists() {
                assert!(Instant::now() < deadline, "background git did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            let prompt = std::fs::read_to_string(paths::home().join("started")).unwrap();
            assert_eq!(prompt, "0", "the check must run through imacs-core's guarded git");
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
                    commit: None,
                    revision: app.runtime.integration_revision - 1,
                    plan: app.runtime.plans[index].id.clone(),
                    task: checked,
                    into: "integration".into(),
                    valid: Some(true),
                    retry: false,
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
    fn a_git_object_fifo_stops_the_check_at_its_time_limit_and_it_runs_again_later() {
        with_app(
            "app::integrations::tests::a_git_object_fifo_stops_the_check_at_its_time_limit_and_it_runs_again_later",
            |app, _| {
                let (_dir, repo) = repository();
                git::git(&repo, &["branch", "integration"]).unwrap();
                request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Waiting ref", "integration":"integration", "tasks":[
                        {"id":"a", "repo":repo, "branch":"task-a"}, {"id":"b", "depends_on":["a"]}]}),
                );
                app.runtime.drain_actions();
                let index = app.runtime.selected_plan;
                let key = (app.runtime.plans[index].id.clone(), "a".to_string());
                app.runtime.plans[index].tasks[0].status = TaskStatus::Done;
                app.runtime.plans[index].tasks[0].integrated = Some("integration".into());
                // Ref FIFOs are refused before git starts. A loose object FIFO still exercises
                // the bounded background runner without blocking the UI.
                git::git(&repo, &["branch", "task-a", "integration"]).unwrap();
                let object = git::git(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
                let fifo = repo.join(".git/objects").join(&object[..2]).join(&object[2..]);
                let saved = fifo.with_extension("saved");
                std::fs::rename(&fifo, &saved).unwrap();
                assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
                let limit = Duration::from_secs(3);
                git::set_quick_timeout(limit);
                let started = Instant::now();
                app.runtime.refresh_integrations();
                assert!(app.runtime.integration_checks.contains(&key));
                while !app.runtime.integration_checks.is_empty() {
                    assert!(started.elapsed() < limit + Duration::from_secs(15), "the check outlived git's time limit");
                    std::thread::sleep(Duration::from_millis(10));
                    app.runtime.refresh_integrations();
                }
                assert!(started.elapsed() >= limit, "git must wait on the FIFO for this test to mean anything");
                assert!(!app.runtime.plans[index].tasks[0].integration_verified);
                assert_eq!(app.runtime.plans[index].tasks[0].integrated.as_deref(), Some("integration"));
                assert!(app.runtime.plans[index].ready().is_empty());
                app.runtime.refresh_integrations();
                assert!(app.runtime.integration_checks.is_empty(), "a check git did not finish waits to run again");

                // Once the wait is over, it runs again without a ref event, and finds the branch.
                std::fs::remove_file(&fifo).unwrap();
                std::fs::rename(&saved, &fifo).unwrap();
                let at = app.runtime.integration_retries.get_mut(&key).expect("the check must be rescheduled");
                assert!(*at > Instant::now());
                *at = Instant::now();
                app.runtime.refresh_integrations();
                assert!(app.runtime.integration_checks.contains(&key));
                let deadline = Instant::now() + Duration::from_secs(10);
                while !app.runtime.plans[index].tasks[0].integration_verified {
                    assert!(Instant::now() < deadline, "the check that ran again did not finish");
                    std::thread::sleep(Duration::from_millis(10));
                    app.runtime.refresh_integrations();
                }
                assert!(!app.runtime.integration_retries.contains_key(&key));
                assert_eq!(app.runtime.plans[index].ready().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["b"]);
            },
        );
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
                    commit: None,
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
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
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
                // The hook holds the first integration until `release` exists. It must never outlive
                // the test: a failed test drops its folder (often before the hook sees `release`), so
                // the hook also stops when the folder is gone, and after a few minutes at most.
                // Otherwise every failure would leave a loop starting processes and loading the machine.
                std::fs::write(
                    &hook,
                    format!(
                        "#!/bin/sh\ntouch '{}'\nattempts=0\nwhile [ ! -f '{}' ]; do\n  [ -d '{}' ] || exit 1\n  \
                         attempts=$((attempts + 1))\n  [ \"$attempts\" -lt 6000 ] || exit 1\n  sleep 0.02\ndone\n",
                        started.display(),
                        release.display(),
                        dir1.path().display()
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
                // The first integration cannot finish before `release` exists, so a second integration
                // that waited for it would never arrive in time. The bound only has to outlast a loaded
                // machine: while the whole suite runs, a single git call can take half a second.
                const BOUND: Duration = Duration::from_secs(25);
                app.runtime.integrate(indices[0], "a", None);
                let deadline = Instant::now() + BOUND;
                while !started.exists() {
                    if Instant::now() >= deadline {
                        std::fs::write(&release, "").unwrap();
                        panic!("first integration did not reach its hook");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.integrate(indices[1], "a", None);
                let other = app.runtime.integrate_rx.recv_timeout(BOUND);
                std::fs::write(&release, "").unwrap();
                let other = other.expect("a second repository must not wait for the first repository's integration");
                assert_eq!(other.plan, app.runtime.plans[indices[1]].id);
                assert!(other.result.is_ok());
                app.runtime.finish_integration(other);
                let first = app.runtime.integrate_rx.recv_timeout(BOUND).unwrap();
                assert_eq!(first.plan, app.runtime.plans[indices[0]].id);
                assert!(first.result.is_ok());
                app.runtime.finish_integration(first);
                assert!(app.runtime.plans[indices[0]].tasks[0].integrated.is_some());
                assert!(app.runtime.plans[indices[1]].tasks[0].integrated.is_some());
            },
        );
    }
}
