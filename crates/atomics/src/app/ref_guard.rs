use super::*;
use crate::ref_guard::Exempt;

impl State {
    pub(super) fn integration_claim_alert(&mut self, plan: &str, task: &str, worktree: &Path, details: String) {
        let mut owners: Vec<_> = self
            .plans
            .iter()
            .flat_map(|plan| {
                plan.tasks
                    .iter()
                    .filter(|task| {
                        task.worktree.as_ref().is_some_and(|path| {
                            path == worktree || path.canonicalize().ok().as_deref() == Some(worktree)
                        })
                    })
                    .map(move |task| (plan.id.clone(), task.id.clone()))
            })
            .collect();
        let attempted = (plan.to_string(), task.to_string());
        if !owners.contains(&attempted) {
            owners.push(attempted);
        }
        for (plan, task) in owners {
            self.task_ref_alert_at(&plan, &task, details.clone(), None);
        }
    }

    /// Branches whose moves are legitimate work: those of live agents, unfinished tasks and
    /// worktrees being made. A done task's branch is finished, and moving it is restored. Refs ATOMICS
    /// writes itself, such as integration branches, go through [`crate::ref_guard::host_write`]
    /// instead, so an agent moving them is still restored. Runs no git: [`Exempt::resolve`] does that
    /// on the ref guard thread.
    pub(super) fn exempt_branches(&self) -> Exempt {
        let mut exempt = Exempt::default();
        for session in &self.sessions {
            if session.chief || !session.is_agent() || session.status == AgentStatus::Exited {
                continue;
            }
            match &session.ref_guard {
                Some(guard) => exempt.refs.extend(guard.branch.clone().map(|branch| (guard.common.clone(), branch))),
                None => exempt.folders.extend(session.branch().map(|b| (session.root(), format!("refs/heads/{b}")))),
            }
        }
        for task in self.plans.iter().flat_map(|plan| &plan.tasks).filter(|task| task.status != TaskStatus::Done) {
            if let (Some(worktree), Some(branch)) = (&task.worktree, &task.branch) {
                exempt.folders.push((worktree.clone(), format!("refs/heads/{branch}")));
            }
        }
        for (common, branch, _) in &self.creating {
            exempt.refs.push((common.clone(), format!("refs/heads/{branch}")));
        }
        exempt
    }

    /// Check the completed turn and save a fresh baseline before the next one, including
    /// turns typed directly into a tile. Exit checks also catch an agent too quick to show busy.
    pub(super) fn check_refs(&self, index: usize) {
        self.queue_ref_check(index, self.exempt_branches());
    }

    /// Checks every guard, with `exempt` gathered while the agents that just exited or closed
    /// still counted as live: their last commits are accepted before their branches stop being exempt.
    pub(super) fn check_all_refs(&self, exempt: &Exempt) {
        for index in 0..self.sessions.len() {
            self.queue_ref_check(index, exempt.clone());
        }
    }

    fn queue_ref_check(&self, index: usize, exempt: Exempt) {
        let Some(guard) = self.sessions[index].ref_guard.clone() else { return };
        let (id, actions) = (self.sessions[index].id.clone(), self.action_tx.clone());
        self.ref_checks.run(move || {
            let alerts =
                guard.check(&exempt).unwrap_or_else(|error| vec![format!("Ref guard check FAILED: {error:#}")]);
            if !alerts.is_empty() {
                let generation = guard.alert_generation();
                let _ = actions.send(Box::new(move |state| state.ref_alert_at(&id, alerts.join("\n"), generation)));
            }
        });
    }

    #[cfg(test)]
    pub(super) fn ref_alert(&mut self, id: &str, details: String) {
        let generation = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.ref_guard.as_ref())
            .and_then(|g| g.alert_generation());
        self.ref_alert_at(id, details, generation);
    }

    pub(super) fn ref_alert_at(&mut self, id: &str, details: String, generation: Option<String>) {
        let summary = format!("SANDBOX REF GUARD: {details}");
        if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions[index].last_message = Some(summary.clone());
        }
        let mut owners = Vec::new();
        for (index, plan) in self.plans.iter_mut().enumerate() {
            for task in plan.tasks.iter_mut().filter(|task| task.session.as_deref() == Some(id)) {
                if task.ref_alert.is_none() {
                    task.ref_status_before_alert = Some(task.status);
                }
                task.ref_alert = Some(summary.clone());
                task.ref_alert_generation = generation.clone().or_else(|| Some(uuid::Uuid::new_v4().to_string()));
                task.integration_verified = false;
                if task.status != TaskStatus::Done {
                    task.set_status(TaskStatus::Blocked);
                }
                task.notes.push(summary.clone());
                owners.push(index);
            }
        }
        for index in owners {
            self.changed_plans.insert(self.plans[index].id.clone());
            self.save_plan(index);
        }
        // Reports wake the chief's normal atomics_wait loop, even after the tile was closed.
        self.report(Some(id), "blocked", &summary);
    }

    #[cfg(test)]
    pub(super) fn task_ref_alert(&mut self, plan: &str, task: &str, details: String) {
        self.task_ref_alert_at(plan, task, details, None);
    }

    pub(super) fn task_ref_alert_at(&mut self, plan: &str, task: &str, details: String, generation: Option<String>) {
        let Some(index) = self.plans.iter().position(|p| p.id == plan) else { return };
        let Some(task) = self.plans[index].task_mut(task) else { return };
        if let Some(session) = task.session.clone() {
            self.ref_alert_at(&session, details, generation);
        } else {
            let summary = format!("SANDBOX REF GUARD: {details}");
            if task.ref_alert.is_none() {
                task.ref_status_before_alert = Some(task.status);
            }
            task.ref_alert = Some(summary.clone());
            task.ref_alert_generation = Some(uuid::Uuid::new_v4().to_string());
            task.integration_verified = false;
            if task.status != TaskStatus::Done {
                task.set_status(TaskStatus::Blocked);
            }
            task.notes.push(summary);
            self.changed_plans.insert(self.plans[index].id.clone());
            self.save_plan(index);
        }
    }

    pub(super) fn acknowledge_refs(&mut self, index: usize, task_index: usize, seen: Option<String>) {
        let task = &self.plans[index].tasks[task_index];
        if task.ref_alert_generation != seen {
            return;
        }
        if let Some(guard) = task
            .session
            .as_ref()
            .and_then(|id| self.sessions.iter().find(|s| &s.id == id))
            .and_then(|s| s.ref_guard.as_ref())
            && let Some(seen) = &seen
        {
            guard.acknowledge(seen);
        }
        let task = &mut self.plans[index].tasks[task_index];
        task.ref_alert = None;
        task.ref_alert_generation = None;
        if let Some(status) = task.ref_status_before_alert.take() {
            task.set_status(status);
        }
        self.changed_plans.insert(self.plans[index].id.clone());
        self.save_plan(index);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, session, with_app};
    use super::*;

    fn forged_integration_claim(app: &mut Desktop<'_>, existing: bool) {
        let (dir, repo) = repository();
        let task = dir.path().join("task");
        let attacker = dir.path().join("sandbox");
        let attacker_alias = dir.path().join("sandbox-alias");
        let destination = dir.path().join("integration");
        git::add_worktree(&repo, &task, "task", "main").unwrap();
        git::add_worktree(&repo, &attacker, "sandbox", "main").unwrap();
        std::os::unix::fs::symlink(&attacker, &attacker_alias).unwrap();
        if existing {
            git::add_worktree(&repo, &destination, "integration", "main").unwrap();
        }
        let home = paths::atomics_home();
        let pins = home.join("git-pins");
        crate::sandbox::pin_git(&pins, "atomics-attacker", &attacker).unwrap();
        crate::sandbox_ownership::record_pins(&home, "atomics-attacker", &pins).unwrap();
        app.runtime.config.worktree_path = "{repo_parent}/integration".into();
        app.runtime.plans = vec![Plan {
            id: "p".into(),
            project: Some(repo.clone()),
            integration: Some("integration".into()),
            tasks: vec![
                Task {
                    id: "victim".into(),
                    branch: Some("task".into()),
                    worktree: Some(task.clone()),
                    status: TaskStatus::Review,
                    ..Default::default()
                },
                Task {
                    id: "attacker".into(),
                    branch: Some("sandbox".into()),
                    worktree: Some(attacker_alias),
                    status: TaskStatus::Running,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];
        std::fs::write(task.join("feature"), "uncommitted work").unwrap();
        std::fs::write(attacker.join(".gitattributes"), "* merge=union\n").unwrap();
        let source = git::commit_of(&repo, "refs/heads/task").unwrap();
        let target = git::commit_of(&repo, "refs/heads/integration").unwrap();
        git::git(&attacker, &["symbolic-ref", "HEAD", "refs/heads/integration"]).unwrap();
        app.runtime.integrate(0, "victim", None);
        let done = app
            .runtime
            .integrate_rx
            .recv_timeout(crate::test_support::scaled_timeout(std::time::Duration::from_secs(10)))
            .unwrap();
        assert!(done.result.as_ref().unwrap_err().contains("claims integration ref"));
        // Reverting before queued UI delivery must not erase the attack's alert.
        git::git(&attacker, &["symbolic-ref", "HEAD", "refs/heads/sandbox"]).unwrap();
        app.runtime.drain_actions();
        app.runtime.finish_integration(done);
        for task in &app.runtime.plans[0].tasks {
            assert!(task.ref_alert.as_ref().unwrap().contains("claims integration ref"));
            assert_eq!(task.status, TaskStatus::Blocked);
        }
        assert_eq!(git::commit_of(&repo, "refs/heads/task").unwrap(), source);
        assert_eq!(git::commit_of(&repo, "refs/heads/integration").unwrap(), target);
        assert!(git::git(&task, &["diff", "--cached"]).unwrap().is_empty());
        assert_eq!(destination.exists(), existing);
        for index in 0..2 {
            let generation = app.runtime.plans[0].tasks[index].ref_alert_generation.clone();
            app.runtime.acknowledge_refs(0, index, generation);
        }
        app.runtime.integrate(0, "victim", None);
        let done = app
            .runtime
            .integrate_rx
            .recv_timeout(crate::test_support::scaled_timeout(std::time::Duration::from_secs(10)))
            .unwrap();
        let result = done.result.as_ref().unwrap();
        assert_eq!(result.worktree, destination);
        assert!(destination.join("feature").exists());
        assert!(!destination.join(".gitattributes").exists());
        app.runtime.finish_integration(done);
        git::unpin(&attacker);
    }

    #[test]
    fn a_sandbox_cannot_claim_the_destination_before_first_integration() {
        with_app("app::ref_guard::tests::a_sandbox_cannot_claim_the_destination_before_first_integration", |app, _| {
            forged_integration_claim(app, false)
        });
    }

    #[test]
    fn a_sandbox_claim_beside_the_real_destination_raises_an_attributable_alert() {
        with_app(
            "app::ref_guard::tests::a_sandbox_claim_beside_the_real_destination_raises_an_attributable_alert",
            |app, _| forged_integration_claim(app, true),
        );
    }

    #[test]
    fn the_host_records_its_integration_path_across_reload_and_template_changes() {
        with_app(
            "app::ref_guard::tests::the_host_records_its_integration_path_across_reload_and_template_changes",
            |app, _| {
                let (dir, repo) = repository();
                let task = dir.path().join("task");
                git::add_worktree(&repo, &task, "task", "main").unwrap();
                app.runtime.config.worktree_path = "{repo_parent}/original-integration".into();
                app.runtime.plans = vec![Plan {
                    id: "p".into(),
                    project: Some(repo.clone()),
                    integration: Some("integration".into()),
                    tasks: vec![Task {
                        id: "t".into(),
                        branch: Some("task".into()),
                        worktree: Some(task.clone()),
                        status: TaskStatus::Review,
                        ..Default::default()
                    }],
                    ..Default::default()
                }];
                for content in ["first", "second"] {
                    std::fs::write(task.join("feature"), content).unwrap();
                    app.runtime.integrate(0, "t", None);
                    let done = app
                        .runtime
                        .integrate_rx
                        .recv_timeout(crate::test_support::scaled_timeout(std::time::Duration::from_secs(10)))
                        .unwrap();
                    assert_eq!(done.result.as_ref().unwrap().worktree, dir.path().join("original-integration"));
                    app.runtime.finish_integration(done);
                    app.runtime.plans[0] = Plan::load(&paths::atomics_home().join("plans/p.json")).unwrap();
                    app.runtime.config.worktree_path = "{repo_parent}/new-integration".into();
                }
                assert_eq!(std::fs::read_to_string(dir.path().join("original-integration/feature")).unwrap(), "second");
                assert!(!dir.path().join("new-integration").exists());
            },
        );
    }

    #[test]
    fn a_legacy_plan_adopts_its_existing_integration_checkout_once() {
        with_app("app::ref_guard::tests::a_legacy_plan_adopts_its_existing_integration_checkout_once", |app, _| {
            let (dir, repo) = repository();
            let task = dir.path().join("task");
            let original = dir.path().join("original-integration");
            git::add_worktree(&repo, &task, "task", "main").unwrap();
            git::add_worktree(&repo, &original, "integration", "main").unwrap();
            app.runtime.config.worktree_path = "{repo_parent}/changed-integration".into();
            app.runtime.plans = vec![Plan {
                id: "p".into(),
                project: Some(repo.clone()),
                integration: Some("integration".into()),
                tasks: vec![Task {
                    id: "t".into(),
                    branch: Some("task".into()),
                    worktree: Some(task.clone()),
                    status: TaskStatus::Review,
                    ..Default::default()
                }],
                ..Default::default()
            }];
            assert!(app.runtime.plans[0].integration_worktrees.is_empty());
            for content in ["first", "second"] {
                std::fs::write(task.join("feature"), content).unwrap();
                app.runtime.integrate(0, "t", None);
                let done = app
                    .runtime
                    .integrate_rx
                    .recv_timeout(crate::test_support::scaled_timeout(std::time::Duration::from_secs(10)))
                    .unwrap();
                assert_eq!(done.result.as_ref().unwrap().worktree, original);
                app.runtime.finish_integration(done);
                app.runtime.plans[0] = Plan::load(&paths::atomics_home().join("plans/p.json")).unwrap();
                let records = &app.runtime.plans[0].integration_worktrees;
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].path, original);
                app.runtime.config.worktree_path = "{repo_parent}/changed-again".into();
            }
            assert_eq!(std::fs::read_to_string(original.join("feature")).unwrap(), "second");
            assert!(!dir.path().join("changed-integration").exists());
            assert!(!dir.path().join("changed-again").exists());
        });
    }

    #[test]
    fn a_symlinked_pinned_head_alerts_its_owner_before_git_can_reject_it() {
        with_app(
            "app::ref_guard::tests::a_symlinked_pinned_head_alerts_its_owner_before_git_can_reject_it",
            |app, _| {
                let (dir, repo) = repository();
                let task = dir.path().join("task");
                let sandbox = dir.path().join("sandbox");
                git::add_worktree(&repo, &task, "task", "main").unwrap();
                git::add_worktree(&repo, &sandbox, "sandbox", "main").unwrap();
                let pins_dir = paths::atomics_home().join("git-pins");
                crate::sandbox::pin_git(&pins_dir, "atomics-sandbox", &sandbox).unwrap();
                crate::sandbox_ownership::record_pins(&paths::atomics_home(), "atomics-sandbox", &pins_dir).unwrap();
                let pins = git::pinned(&sandbox).unwrap();
                let head = pins.git_dir.as_ref().unwrap().join("HEAD");
                std::fs::remove_file(&head).unwrap();
                std::os::unix::fs::symlink("/dev/zero", &head).unwrap();
                app.runtime.plans = vec![Plan {
                    id: "p".into(),
                    project: Some(repo),
                    integration: Some("integration".into()),
                    tasks: vec![
                        Task {
                            id: "victim".into(),
                            branch: Some("task".into()),
                            worktree: Some(task),
                            status: TaskStatus::Review,
                            ..Default::default()
                        },
                        Task {
                            id: "owner".into(),
                            branch: Some("sandbox".into()),
                            worktree: Some(sandbox.clone()),
                            status: TaskStatus::Running,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }];
                app.runtime.integrate(0, "victim", None);
                assert!(app.runtime.integrating.is_empty());
                for task in &app.runtime.plans[0].tasks {
                    assert!(task.ref_alert.as_ref().unwrap().contains("unsafe admin metadata"));
                }
                std::fs::remove_file(&head).unwrap();
                std::fs::write(head, "ref: refs/heads/sandbox\n").unwrap();
                git::unpin(&sandbox);
            },
        );
    }

    #[test]
    fn missing_task_folder_checks_its_snapshot_without_changing_host_head() {
        with_app(
            "app::ref_guard::tests::missing_task_folder_checks_its_snapshot_without_changing_host_head",
            |app, _| {
                let (dir, repo) = repository();
                let task = dir.path().join("task");
                git::add_worktree(&repo, &task, "task", "main").unwrap();
                std::fs::write(task.join("feature"), "feature").unwrap();
                git::commit_all(&task, "feature").unwrap();
                let home = dir.path().join("state");
                let pins = home.join("git-pins");
                crate::sandbox::pin_git(&pins, "atomics-task", &task).unwrap();
                crate::sandbox_ownership::record_pins(&home, "atomics-task", &pins).unwrap();
                crate::ref_guard::RefGuard::open(&task, home.join("ref-guards/atomics-task.json")).unwrap().unwrap();
                git::git(&repo, &["worktree", "remove", task.to_str().unwrap()]).unwrap();
                let checked = crate::ref_guard::check_integration(
                    &app.runtime.ref_checks,
                    None,
                    &home,
                    &repo,
                    &task,
                    Exempt::default(),
                )
                .unwrap();
                assert!(checked.is_empty(), "{checked:?}");
                assert_eq!(git::git(&repo, &["symbolic-ref", "HEAD"]).unwrap().trim(), "refs/heads/main");
                let into = dir.path().join("integration");
                git::integrate(&repo, &task, "task", "finished", "integration", &into, "main").unwrap();
                assert!(into.join("feature").exists());
                let tip = git::commit_of(&repo, "refs/heads/main").unwrap().unwrap();
                git::git(&repo, &["update-ref", "refs/heads/unexpected", &tip]).unwrap();
                let alerts = crate::ref_guard::check_integration(
                    &app.runtime.ref_checks,
                    None,
                    &home,
                    &repo,
                    &task,
                    Exempt::default(),
                )
                .unwrap();
                assert!(alerts.iter().any(|a| a.contains("Restored refs/heads/unexpected")), "{alerts:?}");
                assert_eq!(git::git(&repo, &["symbolic-ref", "HEAD"]).unwrap().trim(), "refs/heads/main");
                git::unpin(&task);
            },
        );
    }

    #[test]
    fn missing_legacy_pin_has_explicit_recovery_and_expired_refusals_are_reloaded() {
        with_app(
            "app::ref_guard::tests::missing_legacy_pin_has_explicit_recovery_and_expired_refusals_are_reloaded",
            |_app, _| {
                let (dir, repo) = repository();
                let original = git::resolve_pins(&repo).unwrap();
                let home = dir.path().join("state");
                let pins = home.join("git-pins");
                std::fs::create_dir_all(&pins).unwrap();
                let file = pins.join("atomics-legacy.json");
                let ledger = home.join("sandbox-ownership.json");
                let legacy = serde_json::json!({"atomics-legacy": {"pins": pins}});
                std::fs::write(&ledger, serde_json::to_vec(&legacy).unwrap()).unwrap();
                let error = format!("{:#}", crate::sandbox::load_owned_pins(&home).unwrap_err());
                assert!(
                    error.contains("atomics sandbox forget-pin atomics-legacy --confirm-worktree-removed"),
                    "{error}"
                );
                assert!(git::git(&repo, &["status"]).is_err());
                assert!(crate::sandbox::forget_legacy_pin(&home, "missing-sbx", "atomics-legacy", false).is_err());
                assert!(git::git(&repo, &["status"]).is_err());
                let bin = dir.path().join("bin");
                std::fs::create_dir(&bin).unwrap();
                let sbx = crate::sandbox::fake::install(&bin);
                std::fs::create_dir_all(bin.join("sandboxes")).unwrap();
                let listed = bin.join("sandboxes/atomics-legacy");
                std::fs::write(
                    &listed,
                    serde_json::to_vec(&atomics_core::isolation::Listed {
                        name: "atomics-legacy".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                )
                .unwrap();
                assert!(
                    crate::sandbox::forget_legacy_pin(&home, sbx.to_str().unwrap(), "atomics-legacy", true).is_err(),
                    "a live sandbox must be removed first"
                );
                std::fs::remove_file(listed).unwrap();
                crate::sandbox::forget_legacy_pin(&home, "missing-sbx", "atomics-legacy", true).unwrap();
                git::git(&repo, &["status"]).unwrap();
                // A separate process can remove the ledger entry and its file too.
                std::fs::write(&ledger, serde_json::to_vec(&legacy).unwrap()).unwrap();
                assert!(crate::sandbox::load_owned_pins(&home).is_err());
                std::fs::write(&ledger, b"{}").unwrap();
                crate::sandbox::load_owned_pins(&home).unwrap();
                git::git(&repo, &["status"]).unwrap();
                // Repairing a legacy file migrates it and also clears the old refusal.
                std::fs::write(&ledger, serde_json::to_vec(&legacy).unwrap()).unwrap();
                assert!(crate::sandbox::load_owned_pins(&home).is_err());
                let saved = git::resolve_pins(&repo); // blocked until the original host record is repaired
                assert!(saved.is_err());
                std::fs::write(&file, serde_json::to_vec(&original).unwrap()).unwrap();
                crate::sandbox::load_owned_pins(&home).unwrap();
                git::git(&repo, &["status"]).unwrap();
                assert!(crate::sandbox::forget_legacy_pin(&home, "missing-sbx", "atomics-legacy", true).is_err());
                git::unpin(&repo);
            },
        );
    }

    #[test]
    fn removing_an_unreadable_custom_pin_directory_expires_its_refusals() {
        with_app(
            "app::ref_guard::tests::removing_an_unreadable_custom_pin_directory_expires_its_refusals",
            |_app, _| {
                let (dir, repo) = repository();
                let home = dir.path().join("state");
                std::fs::create_dir(&home).unwrap();
                let pins = home.join("old-pins");
                // ENOTDIR is deterministic even when tests run with elevated permissions.
                std::fs::write(&pins, "not a directory").unwrap();
                let ledger = home.join("sandbox-ownership.json");
                std::fs::write(&ledger, json!({"atomics-legacy": {"pins": pins}}).to_string()).unwrap();
                assert!(crate::sandbox::load_owned_pins(&home).is_err());
                assert!(git::git(&repo, &["status"]).is_err());
                // Another process removes both the obsolete ledger entry and its directory.
                std::fs::write(&ledger, b"{}").unwrap();
                std::fs::remove_file(&pins).unwrap();
                crate::sandbox::load_owned_pins(&home).unwrap();
                git::git(&repo, &["status"]).unwrap();
            },
        );
    }

    #[test]
    fn integration_cannot_overtake_a_ref_alert_waiting_for_the_ui() {
        with_app("app::ref_guard::tests::integration_cannot_overtake_a_ref_alert_waiting_for_the_ui", |app, _| {
            let (dir, repo) = repository();
            let task = dir.path().join("task");
            git::add_worktree(&repo, &task, "task", "main").unwrap();
            let guard = crate::ref_guard::Shared::new(
                crate::ref_guard::RefGuard::open(&task, dir.path().join("snapshot.json")).unwrap().unwrap(),
            );
            let initial = git::commit_of(&repo, "refs/heads/main").unwrap().unwrap();
            git::git(&repo, &["update-ref", "refs/heads/unexpected", &initial]).unwrap();
            assert!(!guard.check(&Exempt::default()).unwrap().is_empty());
            // The guard has recovered the ref, but its queued UI alert has not been delivered.
            let home = paths::atomics_home();
            let checked = crate::ref_guard::check_integration(
                &app.runtime.ref_checks,
                Some(guard.clone()),
                &home,
                &repo,
                &task,
                Exempt::default(),
            );
            assert!(format!("{:#}", checked.unwrap_err()).contains("unacknowledged"));
            let mut sandboxed = session("s", AgentKind::Generic, AgentStatus::Done, &task);
            sandboxed.ref_guard = Some(guard.clone());
            app.runtime.sessions = vec![sandboxed];
            app.runtime.plans = vec![Plan {
                id: "p".into(),
                tasks: vec![Task { id: "t".into(), session: Some("s".into()), ..Default::default() }],
                ..Default::default()
            }];
            app.runtime.ref_alert("s", "first alert".into());
            let first = app.runtime.plans[0].tasks[0].ref_alert_generation.clone();
            // A2 is detected on the guard worker but is still queued behind the user's A1 acknowledgement.
            git::git(&repo, &["update-ref", "refs/heads/second-unexpected", &initial]).unwrap();
            assert!(!guard.check(&Exempt::default()).unwrap().is_empty());
            app.runtime.acknowledge_refs(0, 0, first.clone());
            assert!(
                crate::ref_guard::check_integration(
                    &app.runtime.ref_checks,
                    Some(guard.clone()),
                    &home,
                    &repo,
                    &task,
                    Exempt::default()
                )
                .is_err()
            );
            app.runtime.ref_alert("s", "second alert".into());
            app.runtime.acknowledge_refs(0, 0, first);
            assert!(app.runtime.plans[0].tasks[0].ref_alert.is_some(), "a stale UI command must not dismiss A2");
            app.runtime.task_action(0, 0, "acknowledge_refs");
            assert!(
                crate::ref_guard::check_integration(
                    &app.runtime.ref_checks,
                    Some(guard),
                    &home,
                    &repo,
                    &task,
                    Exempt::default()
                )
                .unwrap()
                .is_empty()
            );
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn acknowledgement_restores_the_status_saved_before_an_alert_across_reload() {
        with_app(
            "app::ref_guard::tests::acknowledgement_restores_the_status_saved_before_an_alert_across_reload",
            |app, _| {
                for status in [TaskStatus::Review, TaskStatus::Running, TaskStatus::Done] {
                    app.runtime.plans = vec![Plan {
                        id: "p".into(),
                        tasks: vec![Task { id: "t".into(), status, ..Default::default() }],
                        ..Default::default()
                    }];
                    app.runtime.task_ref_alert("p", "t", "moved".into());
                    let saved = serde_json::to_vec(&app.runtime.plans[0]).unwrap();
                    app.runtime.plans[0] = serde_json::from_slice(&saved).unwrap();
                    let seen = app.runtime.plans[0].tasks[0].ref_alert_generation.clone();
                    app.runtime.acknowledge_refs(0, 0, seen);
                    let task = &app.runtime.plans[0].tasks[0];
                    assert!(task.ref_alert.is_none());
                    assert_eq!(task.status, status);
                }
            },
        );
    }

    #[test]
    fn broken_owned_pins_block_startup_doctor_template_and_the_tile() {
        use atomics_ui::Backend;
        super::super::tests::with_login_state(
            "app::ref_guard::tests::broken_owned_pins_block_startup_doctor_template_and_the_tile",
            |root| {
                let repo = root.join("repo");
                let task = root.join("task");
                std::fs::create_dir(&repo).unwrap();
                for args in [
                    &["init", "-qb", "main"][..],
                    &[
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.org",
                        "commit",
                        "--allow-empty",
                        "-qm",
                        "initial",
                    ],
                ] {
                    git::git(&repo, args).unwrap();
                }
                git::add_worktree(&repo, &task, "task", "main").unwrap();
                let home = root.join("state");
                let pins = home.join("git-pins");
                crate::sandbox::pin_git(&pins, "atomics-task", &task).unwrap();
                crate::sandbox_ownership::record_pins(&home, "atomics-task", &pins).unwrap();
                crate::sandbox_ownership::created(&home, "atomics-task", crate::sandbox_ownership::Resource::Sandbox)
                    .unwrap();
                git::unpin(&task.canonicalize().unwrap());
                std::fs::write(pins.join("atomics-task.json"), "broken json").unwrap();
                vec![("ATOMICS_TEST_PIN_ERROR", "1".into())]
            },
            |mut state, _| {
                let task = paths::home().join("task");
                assert!(git::git_refusal(&task).unwrap().contains("host git is blocked"));
                assert!(git::git(&task, &["status"]).is_err());
                state.sessions = vec![session("s", AgentKind::Generic, AgentStatus::Idle, &task)];
                let warning = state.backend().tile(0).unwrap().warning.unwrap();
                assert!(warning.contains("Unreadable sandbox git pin"), "{warning}");
                let doctor = crate::doctor::json(None, std::slice::from_ref(&task)).to_string();
                assert!(doctor.contains("sandbox git pins") && doctor.contains("host git is blocked"), "{doctor}");
                let error =
                    crate::sandbox::template::run(&task.display().to_string(), None, "codex", None, None).unwrap_err();
                assert!(format!("{error:#}").contains("Unreadable sandbox git pin"));
                state.sessions.clear();
                state.shutdown();
            },
        );
    }

    #[test]
    fn a_broken_legacy_pin_without_a_ledger_copy_blocks_all_host_git() {
        super::super::tests::with_login_state(
            "app::ref_guard::tests::a_broken_legacy_pin_without_a_ledger_copy_blocks_all_host_git",
            |root| {
                let home = root.join("state");
                let pins = home.join("git-pins");
                std::fs::create_dir_all(&pins).unwrap();
                std::fs::write(pins.join("atomics-legacy.json"), "broken json").unwrap();
                std::fs::write(
                    home.join("sandbox-ownership.json"),
                    serde_json::to_vec(&json!({
                        "atomics-legacy": {"sandbox":true, "pins":pins}
                    }))
                    .unwrap(),
                )
                .unwrap();
                vec![("ATOMICS_TEST_PIN_ERROR", "1".into())]
            },
            |mut state, _| {
                assert!(git::pinned_all().is_empty());
                let error = git::git(&paths::home(), &["status"]).unwrap_err();
                assert!(format!("{error:#}").contains("Unreadable sandbox git pin"));
                assert!(git::git_refusal(Path::new("/")).is_some());
                state.shutdown();
            },
        );
    }

    #[test]
    fn a_failed_ref_check_blocks_integration_before_any_git_write() {
        with_app("app::ref_guard::tests::a_failed_ref_check_blocks_integration_before_any_git_write", |app, ctx| {
            let (dir, repo) = repository();
            let task = dir.path().join("task");
            git::add_worktree(&repo, &task, "task", "main").unwrap();
            let guard = crate::ref_guard::RefGuard::open(&task, dir.path().join("snapshot.json")).unwrap().unwrap();
            let mut sandboxed = session("s", AgentKind::Generic, AgentStatus::Done, &task);
            sandboxed.sandbox = Some("atomics-s".into());
            sandboxed.ref_guard = Some(crate::ref_guard::Shared::new(guard));
            app.runtime.sessions = vec![sandboxed];
            app.runtime.plans = vec![Plan {
                id: "p".into(),
                project: Some(repo.clone()),
                tasks: vec![Task {
                    id: "t".into(),
                    branch: Some("task".into()),
                    worktree: Some(task.clone()),
                    session: Some("s".into()),
                    status: TaskStatus::Review,
                    ..Default::default()
                }],
                ..Default::default()
            }];
            std::fs::write(task.join("pending.txt"), "must stay uncommitted\n").unwrap();
            std::fs::write(repo.join(".git/refs/heads/broken"), "invalid object\n").unwrap();
            app.runtime.integrate(0, "t", None);
            super::super::sandbox_tests::tick_until(app, ctx, "failed integration guard", |app| {
                app.runtime.integrating.is_empty() && app.runtime.plans[0].tasks[0].ref_alert.is_some()
            });
            assert!(app.runtime.plans[0].tasks[0].ref_alert.as_ref().unwrap().contains("FAILED"));
            assert!(!git::branch_exists(&repo, "atomics/plan-p"));
            assert!(git::git(&task, &["diff", "--cached"]).unwrap().is_empty());
            app.runtime.report(Some("s"), "done", "try again");
            app.runtime.plans[0].tasks[0].status = TaskStatus::Review;
            app.runtime.integrate(0, "t", None);
            assert!(app.runtime.integrating.is_empty(), "an unacknowledged alert prevents integration");
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn checks_run_off_the_state_lock_and_restore_branches_of_done_tasks_and_exited_agents() {
        with_app(
            "app::ref_guard::tests::checks_run_off_the_state_lock_and_restore_branches_of_done_tasks_and_exited_agents",
            |app, _| {
                let (dir, repo) = repository();
                let worktree = |name: &str| dir.path().join(name);
                for name in ["guarded", "live", "gone", "running", "done"] {
                    git::add_worktree(&repo, &worktree(name), name, "main").unwrap();
                }
                let guarded = worktree("guarded");
                let file = dir.path().join("snapshot.json");
                let guard = crate::ref_guard::RefGuard::open(&guarded, file).unwrap().unwrap();
                let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Working, &guarded);
                sandboxed.ref_guard = Some(crate::ref_guard::Shared::new(guard));
                let peer = |id: &str, status| {
                    let mut peer = session(id, AgentKind::Generic, status, &worktree(id));
                    peer.worktree = git::worktree_of(&peer.cwd);
                    peer
                };
                app.runtime.sessions =
                    vec![sandboxed, peer("live", AgentStatus::Idle), peer("gone", AgentStatus::Exited)];
                let task = |id: &str, status| Task {
                    id: id.into(),
                    worktree: Some(worktree(id)),
                    branch: Some(id.into()),
                    status,
                    ..Default::default()
                };
                let tasks = vec![task("running", TaskStatus::Running), task("done", TaskStatus::Done)];
                app.runtime.plans = vec![Plan { id: "guard-plan".into(), tasks, ..Default::default() }];
                let original = git::git(&repo, &["rev-parse", "main"]).unwrap();
                // A new commit, and every other branch moved to it, as an agent in the sandbox could.
                git::git(&guarded, &["commit", "--allow-empty", "-qm", "work"]).unwrap();
                let moved = git::git(&guarded, &["rev-parse", "HEAD"]).unwrap();
                for branch in ["running", "done", "live", "gone"] {
                    git::git(&repo, &["update-ref", &format!("refs/heads/{branch}"), moved.trim()]).unwrap();
                }

                // With the ref guard thread busy and git unusable meanwhile, a check is only queued: any git
                // the state thread ran for it would fail, and leave exempt branches out or fail the check.
                let (release, wait) = std::sync::mpsc::channel::<()>();
                app.runtime.ref_checks.run(move || {
                    let _ = wait.recv();
                });
                std::fs::rename(repo.join(".git"), repo.join(".git-away")).unwrap();
                app.runtime.check_refs(0);
                std::fs::rename(repo.join(".git-away"), repo.join(".git")).unwrap();
                release.send(()).unwrap();

                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[0].last_message.as_deref().is_some_and(|m| m.contains("SANDBOX REF GUARD"))
                {
                    assert!(Instant::now() < deadline, "the check never reported");
                    app.runtime.drain_actions();
                    std::thread::sleep(Duration::from_millis(10));
                }
                let alert = app.runtime.sessions[0].last_message.clone().unwrap();
                assert!(
                    alert.contains("Restored refs/heads/done") && alert.contains("Restored refs/heads/gone"),
                    "{alert}"
                );
                for kept in ["running", "live", "guarded"] {
                    assert!(!alert.contains(&format!("refs/heads/{kept}")), "{alert}");
                }
                assert_eq!(git::git(&repo, &["rev-parse", "done"]).unwrap(), original);
                assert_eq!(git::git(&repo, &["rev-parse", "gone"]).unwrap(), original);
                assert_eq!(git::git(&repo, &["rev-parse", "running"]).unwrap(), moved);
                assert_eq!(git::git(&repo, &["rev-parse", "live"]).unwrap(), moved);
                app.runtime.sessions.clear();
            },
        );
    }
    #[test]
    fn integration_branches_atomics_makes_survive_older_guards_and_agent_branches_do_not() {
        with_app(
            "app::ref_guard::tests::integration_branches_atomics_makes_survive_older_guards_and_agent_branches_do_not",
            |app, _| {
                let (dir, repo) = repository();
                let guarded = dir.path().join("guarded");
                git::add_worktree(&repo, &guarded, "guarded", "main").unwrap();
                let guard =
                    crate::ref_guard::RefGuard::open(&guarded, dir.path().join("snapshot.json")).unwrap().unwrap();
                let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Working, &guarded);
                sandboxed.ref_guard = Some(crate::ref_guard::Shared::new(guard));
                app.runtime.sessions = vec![sandboxed];
                let task = |id: &str, branch: &str, status| Task {
                    id: id.into(),
                    repo: Some(repo.to_string_lossy().into_owned()),
                    branch: Some(branch.into()),
                    worktree: Some(dir.path().join(branch)),
                    status,
                    ..Default::default()
                };
                let running = Task { worktree: Some(guarded.clone()), ..task("a", "guarded", TaskStatus::Running) };
                app.runtime.plans = vec![Plan { id: "first".into(), tasks: vec![running], ..Default::default() }];

                // A check captured before the next plan exists must preserve host-created branches.
                // Integration now waits for that check before writing its integration branch.
                let (release, wait) = std::sync::mpsc::channel::<()>();
                app.runtime.ref_checks.run(move || {
                    let _ = wait.recv();
                });
                app.runtime.check_refs(0);
                let write = crate::ref_guard::host_write(&repo, ["next-task"]);
                git::add_worktree(&repo, &dir.path().join("next-task"), "next-task", "main").unwrap();
                drop(write);
                let tasks = vec![task("b", "next-task", TaskStatus::Review)];
                app.runtime.plans.push(Plan { id: "second".into(), approved: true, tasks, ..Default::default() });
                let into = app.runtime.plans[1].integration_branch();
                app.runtime.integrate(1, "b", None);
                release.send(()).unwrap();
                let integrated = app
                    .runtime
                    .integrate_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(30)))
                    .unwrap();
                assert!(integrated.result.is_ok(), "{:?}", integrated.result);
                app.runtime.finish_integration(integrated);
                // Meanwhile the agent makes a branch of its own.
                git::git(&guarded, &["branch", "agent-made"]).unwrap();
                // And the next check, with both plans in place.
                app.runtime.check_refs(0);
                app.runtime.ref_checks.finish();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[0].last_message.as_deref().is_some_and(|m| m.contains("SANDBOX REF GUARD"))
                {
                    assert!(Instant::now() < deadline, "the check never reported");
                    app.runtime.drain_actions();
                    std::thread::sleep(Duration::from_millis(10));
                }

                assert!(git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{into}")]).is_ok());
                assert!(git::git(&repo, &["rev-parse", "--verify", "refs/heads/agent-made"]).is_err());
                let alert = app.runtime.sessions[0].last_message.clone().unwrap();
                // Never deleted: the agent's branch is kept aside.
                let kept = "refs/atomics/quarantine/snapshot/refs/heads/agent-made@";
                assert!(alert.contains(&format!("Restored refs/heads/agent-made: moved it to {kept}")), "{alert}");
                let quarantined = git::git(&repo, &["for-each-ref", "--format=%(refname)", "refs/atomics/quarantine/"]);
                assert!(quarantined.unwrap().contains(kept), "{alert}");
                assert!(!alert.contains(&into) && !alert.contains("next-task"), "no alert for ATOMICS's refs: {alert}");
                assert!(!alert.contains("Object("), "plain text: {alert}");
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn an_agent_moving_an_open_plans_integration_branch_is_restored_to_what_atomics_merged() {
        with_app(
            "app::ref_guard::tests::an_agent_moving_an_open_plans_integration_branch_is_restored_to_what_atomics_merged",
            |app, _| {
                use crate::ref_guard::{RefGuard, Shared};
                let (dir, repo) = repository();
                let worktree = |name: &str| dir.path().join(name);
                for name in ["guarded", "closed"] {
                    git::add_worktree(&repo, &worktree(name), name, "main").unwrap();
                }
                git::add_worktree(&repo, &worktree("next-task"), "next-task", "main").unwrap();
                let guard = RefGuard::open(&worktree("guarded"), dir.path().join("snapshot.json")).unwrap().unwrap();
                let mut sandboxed =
                    session("sandboxed", AgentKind::Generic, AgentStatus::Working, &worktree("guarded"));
                sandboxed.ref_guard = Some(Shared::new(guard));
                app.runtime.sessions = vec![sandboxed];
                // A sandboxed session that is not running: its snapshot waits on disk to reattach with.
                let closed = paths::atomics_home().join("ref-guards/atomics-closed.json");
                drop(RefGuard::open(&worktree("closed"), closed.clone()).unwrap().unwrap());
                let task = |id: &str, branch: &str, status| Task {
                    id: id.into(),
                    repo: Some(repo.to_string_lossy().into_owned()),
                    branch: Some(branch.into()),
                    worktree: Some(worktree(branch)),
                    status,
                    ..Default::default()
                };
                let running = Task { worktree: Some(worktree("guarded")), ..task("a", "guarded", TaskStatus::Running) };
                app.runtime.plans = vec![
                    Plan { id: "first".into(), tasks: vec![running], ..Default::default() },
                    Plan {
                        id: "second".into(),
                        approved: true,
                        tasks: vec![task("b", "next-task", TaskStatus::Review)],
                        ..Default::default()
                    },
                ];
                // ATOMICS makes the open plan's integration branch after both snapshots were taken.
                let into = app.runtime.plans[1].integration_branch();
                app.runtime.integrate(1, "b", None);
                let integrated = app
                    .runtime
                    .integrate_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(30)))
                    .unwrap();
                assert!(integrated.result.is_ok(), "{:?}", integrated.result);
                app.runtime.finish_integration(integrated);
                let merged = git::git(&repo, &["rev-parse", &into]).unwrap().trim().to_string();

                // The agent points the integration branch, what everything ships from, at a commit of its own.
                let guarded = worktree("guarded");
                let injected = git::git(&guarded, &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "injected"])
                    .unwrap()
                    .trim()
                    .to_string();
                git::git(&guarded, &["update-ref", &format!("refs/heads/{into}"), &injected]).unwrap();
                app.runtime.check_refs(0);
                app.runtime.ref_checks.finish();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.sessions[0].last_message.as_deref().is_some_and(|m| m.contains("SANDBOX REF GUARD"))
                {
                    assert!(Instant::now() < deadline, "the check never reported");
                    app.runtime.drain_actions();
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert_eq!(git::git(&repo, &["rev-parse", &into]).unwrap().trim(), merged, "back to ATOMICS's merge");
                let alert = app.runtime.sessions[0].last_message.clone().unwrap();
                let restored =
                    format!("Restored refs/heads/{into}: from {} back to {}", &injected[..12], &merged[..12]);
                assert!(alert.contains(&restored), "{alert}");

                // The session that was not running reattaches with ATOMICS's merge in its baseline.
                let mut reattached = RefGuard::open(&worktree("closed"), closed).unwrap().unwrap();
                let alerts = reattached.check(&Default::default(), &Default::default()).unwrap();
                assert!(alerts.is_empty(), "{alerts:?}");
                assert_eq!(git::git(&repo, &["rev-parse", &into]).unwrap().trim(), merged);
                app.runtime.sessions.clear();
            },
        );
    }

    /// A repository with a guarded sandbox worktree, a host worktree `other`, and `ahead`, a commit past
    /// `other`'s branch; the guard is open before anything moves.
    fn guarded_with_an_ahead_branch()
    -> (crate::test_support::TempDir, PathBuf, PathBuf, PathBuf, String, crate::ref_guard::Shared) {
        let (dir, repo) = repository();
        let (guarded, other) = (dir.path().join("guarded"), dir.path().join("other"));
        git::add_worktree(&repo, &guarded, "guarded", "main").unwrap();
        git::add_worktree(&repo, &other, "other", "main").unwrap();
        let ahead = git::git(&repo, &["commit-tree", "main^{tree}", "-p", "main", "-m", "ahead"]).unwrap();
        let ahead = ahead.trim().to_string();
        git::git(&repo, &["branch", "ahead", &ahead]).unwrap();
        let guard = crate::ref_guard::RefGuard::open(&guarded, dir.path().join("snapshot.json")).unwrap().unwrap();
        (dir, repo, guarded, other, ahead, crate::ref_guard::Shared::new(guard))
    }

    #[test]
    fn a_host_fast_forward_in_a_worktree_no_sandbox_works_in_is_kept() {
        let (_dir, repo, _guarded, other, ahead, shared) = guarded_with_an_ahead_branch();
        git::git(&other, &["merge", "--ff-only", "ahead"]).unwrap();
        let alerts = shared.check(&Default::default()).unwrap();
        assert!(alerts.is_empty(), "{alerts:?}");
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap().trim(), ahead);
        let quarantined = git::git(&repo, &["for-each-ref", "refs/atomics/quarantine/"]).unwrap();
        assert!(quarantined.trim().is_empty(), "{quarantined}");
    }

    #[test]
    fn a_host_fast_forward_in_a_sandboxed_worktree_is_restored_saying_how_to_keep_it() {
        let (_dir, repo, _guarded, other, ahead, shared) = guarded_with_an_ahead_branch();
        let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
        // Another sandbox works in `other`: its HEAD log is no trace of this machine's, since that agent writes it.
        let pins = git::resolve_pins(&other).unwrap();
        git::pin(pins.clone());
        // Run by hand, as in a terminal on this machine.
        let merged = std::process::Command::new("git")
            .args(["merge", "-q", "--ff-only", "ahead"])
            .current_dir(&other)
            .status()
            .unwrap();
        let alerts = shared.check(&Default::default());
        git::unpin(&pins.work_tree);
        assert!(merged.success());
        let alerts = alerts.unwrap();
        let alert = alerts.iter().find(|alert| alert.starts_with("Restored refs/heads/other:")).expect("restored");
        assert!(alert.contains("It may have been written on this machine, not in the sandbox"), "{alert}");
        let keep = format!("git update-ref refs/heads/other {ahead}");
        assert!(alert.contains(&format!("close this repository's sandboxed sessions, then run `{keep}`")), "{alert}");
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original);
        let kept = format!("refs/atomics/quarantine/snapshot/refs/heads/other@{}", &ahead[..12]);
        assert_eq!(git::git(&repo, &["rev-parse", &kept]).unwrap().trim(), ahead);

        // The command it gives keeps the change once no guard is open.
        drop(shared);
        let command = alert.rsplit('`').nth(1).unwrap();
        git::git(&repo, &command.split(' ').skip(1).collect::<Vec<_>>()).unwrap();
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap().trim(), ahead);
    }

    /// Puts `text` on session 0's terminal screen, as its agent would draw it there.
    fn show(state: &mut State, cwd: &Path, text: &str) {
        let terminal = crate::terminal::Terminal::spawn(
            state.wake.clone(),
            state.theme.terminal_colors(),
            "/bin/sh".into(),
            vec!["-c".into(), format!("printf '%s' '{text}'; exec sleep 600")],
            cwd,
            std::collections::HashMap::new(),
            100,
        )
        .unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
        while !terminal.screen_text(60).contains(text) {
            assert!(Instant::now() < deadline, "screen did not appear");
            state.wake.wait_until(deadline.min(Instant::now() + Duration::from_millis(100)));
        }
        state.sessions[0].terminal = Some(terminal);
    }

    /// Runs the queued checks and what they report.
    fn checked(state: &mut State) -> Option<String> {
        state.ref_checks.finish();
        state.drain_actions();
        state.sessions[0].last_message.clone().filter(|message| message.contains("SANDBOX REF GUARD"))
    }

    #[test]
    fn a_ref_the_sandbox_writes_during_a_turn_is_restored_when_the_poll_loop_ends_it() {
        with_app(
            "app::ref_guard::tests::a_ref_the_sandbox_writes_during_a_turn_is_restored_when_the_poll_loop_ends_it",
            |app, _| {
                let (_dir, repo, guarded, _other, ahead, shared) = guarded_with_an_ahead_branch();
                let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
                let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Idle, &guarded);
                sandboxed.sandbox = Some("atomics-sandboxed".into());
                sandboxed.ref_guard = Some(shared);
                app.runtime.sessions = vec![sandboxed];
                let track = |state: &mut State| {
                    state.busy_checked = Instant::now() - Duration::from_secs(1);
                    state.track_busy();
                };
                show(app.runtime, &guarded, "Working (esc to interrupt)");
                track(app.runtime);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                assert_eq!(checked(app.runtime), None, "the turn starts from a clean baseline");

                // The agent moves a branch it does not own, then its turn ends: the status is Done before
                // the check that ending queued runs.
                git::git(&guarded, &["update-ref", "refs/heads/other", &ahead]).unwrap();
                show(app.runtime, &guarded, "Ready");
                app.runtime.sessions[0].busy_absent_since = Some(Instant::now() - Duration::from_secs(1));
                track(app.runtime);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Done);
                let alert = checked(app.runtime).expect("the turn's write was reported");
                assert!(alert.contains("Restored refs/heads/other"), "{alert}");
                assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original);
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn a_ref_written_while_the_session_is_idle_is_restored() {
        with_app("app::ref_guard::tests::a_ref_written_while_the_session_is_idle_is_restored", |app, _| {
            let (_dir, repo, guarded, _other, ahead, shared) = guarded_with_an_ahead_branch();
            let original = git::git(&repo, &["rev-parse", "other"]).unwrap();
            let mut sandboxed = session("sandboxed", AgentKind::Generic, AgentStatus::Idle, &guarded);
            sandboxed.sandbox = Some("atomics-sandboxed".into());
            sandboxed.ref_guard = Some(shared);
            app.runtime.sessions = vec![sandboxed];
            app.runtime.check_refs(0);
            assert_eq!(checked(app.runtime), None);
            // No turn runs, but a process the agent left running in its sandbox still can write.
            git::git(&guarded, &["update-ref", "refs/heads/other", &ahead]).unwrap();
            app.runtime.check_refs(0);
            let alert = checked(app.runtime).expect("the idle write was reported");
            assert!(alert.contains("Restored refs/heads/other"), "{alert}");
            assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap(), original);
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn agent_reports_keep_the_ref_alert_until_the_user_acknowledges_it() {
        with_app("app::ref_guard::tests::agent_reports_keep_the_ref_alert_until_the_user_acknowledges_it", |app, _| {
            let (dir, _repo) = repository();
            let mut sandboxed = session("s", AgentKind::Generic, AgentStatus::NeedsInput, dir.path());
            sandboxed.sandbox = Some("atomics-s".into());
            app.runtime.sessions = vec![sandboxed];
            let task = Task { id: "t".into(), session: Some("s".into()), ..Default::default() };
            app.runtime.plans = vec![Plan { id: "p".into(), tasks: vec![task], ..Default::default() }];
            for status in ["progress", "done"] {
                app.runtime.ref_alert("s", "moved".into());
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Blocked);
                assert!(app.runtime.plans[0].tasks[0].ref_alert.is_some());
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                app.runtime.report(Some("s"), status, "carrying on");
                let task = &app.runtime.plans[0].tasks[0];
                assert!(task.ref_alert.is_some(), "{status}");
                assert_eq!(task.status, TaskStatus::Blocked, "{status}");
                let session = &app.runtime.sessions[0];
                assert_eq!(session.status, AgentStatus::NeedsInput, "{status}");
            }
            app.runtime.task_action(0, 0, "acknowledge_refs");
            assert!(app.runtime.plans[0].tasks[0].ref_alert.is_none());
            assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Review);
            app.runtime.report(Some("s"), "done", "ready for review");
            assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Review);
            app.runtime.sessions.clear();
        });
    }
}
