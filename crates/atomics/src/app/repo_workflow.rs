use super::*;
use atomics_core::repo_workflow::RepoWorkflow;

impl State {
    fn cached_repo_workflow(&self, repo: &Path) -> Option<&RepoWorkflow> {
        self.repo_workflows
            .get(repo)
            .or_else(|| self.project_roots.get(repo).and_then(|root| self.repo_workflows.get(root)))
            .or_else(|| self.project_of(repo).and_then(|root| self.repo_workflows.get(&root)))
    }

    /// The runtime tick holds the lock the UI waits on, so git runs once per folder, not per tick.
    pub(super) fn resolve_project_root(&mut self, dir: &Path) -> PathBuf {
        if let Some(root) = self.project_roots.get(dir) {
            return root.clone();
        }
        let root =
            self.project_of(dir).or_else(|| workspace::project_root(dir).ok()).unwrap_or_else(|| dir.to_path_buf());
        self.project_roots.insert(dir.to_path_buf(), root.clone());
        root
    }

    pub(super) fn project_base_ref(&self, repo: &Path) -> String {
        match self.cached_repo_workflow(repo) {
            Some(workflow) => workflow
                .config
                .table
                .get("base_ref")
                .and_then(|value| value.as_str())
                .unwrap_or(&self.config.base_ref)
                .to_string(),
            None => self.project_config(repo).base_ref,
        }
    }

    /// Task branches can include integrated dependencies. Their own diff starts at the commit
    /// recorded on dispatch, even after the integration branch moves on.
    pub(super) fn diff_base_ref(&self, root: &Path) -> String {
        self.plans
            .iter()
            .flat_map(|plan| plan.tasks.iter().map(move |task| (plan, task)))
            .filter(|(_, task)| {
                let matches = task.worktree.as_deref() == Some(root)
                    || task.session.as_ref().is_some_and(|id| {
                        self.sessions.iter().any(|session| &session.id == id && session.root() == root)
                    });
                matches && task.start_commit.is_some()
            })
            .max_by_key(|(plan, task)| {
                let live = task.session.as_ref().is_some_and(|id| {
                    self.sessions.iter().any(|session| {
                        &session.id == id && session.root() == root && session.status != AgentStatus::Exited
                    })
                });
                (plan.is_open() && live, plan.is_open(), live)
            })
            .and_then(|(_, task)| task.start_commit.clone())
            .unwrap_or_else(|| self.project_base_ref(root))
    }

    pub(super) fn project_config(&self, repo: &Path) -> Config {
        let mut config = self.effective_config();
        config.identity.default = self.project_identity(repo);
        if let Some(workflow) = self.cached_repo_workflow(repo) {
            return workflow.apply(&config);
        }
        let root = self.project_root_of(repo);
        match self.repo_workflows.get(&root) {
            Some(workflow) => workflow.apply(&config),
            None => RepoWorkflow::read(&root).apply(&config),
        }
    }

    /// Reads the repository's settings now rather than from the cache, which can lag by a
    /// reload: a repository that just started requiring sandboxes must never get a bare agent.
    pub(super) fn current_project_config(&self, repo: &Path) -> Config {
        let mut config = self.effective_config();
        config.identity.default = self.project_identity(repo);
        RepoWorkflow::read(&self.project_root_of(repo)).apply(&config)
    }

    pub(super) fn project_identity(&self, repo: &Path) -> atomics_core::identity::Identity {
        let root = self.project_root_of(repo);
        self.config.identity.resolve(&root.to_string_lossy(), self.tickets.workspace_identity.as_ref(), |path| {
            let expanded = paths::expand(path);
            std::fs::canonicalize(&expanded).unwrap_or(expanded).to_string_lossy().into_owned()
        })
    }

    /// Apply a context switch to ATOMICS-created worktrees, off the runtime/UI lock.
    pub(super) fn refresh_worktree_identities(&mut self) {
        if self.tickets.workspace_identity_error.is_some() {
            return;
        }
        let context = (self.config.identity.clone(), self.tickets.workspace_identity.clone(), self.repos.clone());
        if self.identity_context.as_ref() == Some(&context) {
            return;
        }
        let has_identity = |config: &atomics_core::identity::IdentityConfig,
                            ws: Option<&atomics_core::identity::Identity>| {
            std::iter::once(&config.default)
                .chain(config.projects.values())
                .chain(ws)
                .any(atomics_core::identity::Identity::has_git_identity)
        };
        let previously =
            self.identity_context.as_ref().is_some_and(|(config, ws, _)| has_identity(config, ws.as_ref()));
        let enabled = has_identity(&context.0, context.1.as_ref());
        let policy_changed = self.identity_context.as_ref().map_or_else(
            || context.0 != atomics_core::identity::IdentityConfig::default() || context.1.is_some(),
            |(config, ws, _)| config != &context.0 || ws != &context.1,
        );
        self.identity_context = Some(context);
        if policy_changed {
            self.integration_dirty.store(true, Ordering::Relaxed);
        }
        self.management_config.take();
        if !enabled && !previously {
            return;
        }
        let projects: Vec<_> = self.repos.iter().map(|repo| (repo.clone(), self.project_identity(repo))).collect();
        let mut owned = self.identity_worktrees.clone();
        owned.extend(self.plans.iter().flat_map(|plan| &plan.tasks).filter_map(|task| task.worktree.clone()));
        owned
            .extend(self.sessions.iter().filter_map(|session| session.worktree.as_ref().map(|tree| tree.root.clone())));
        let actions = self.action_tx.clone();
        self.identity_updates.run(move || {
            for (repo, identity) in projects {
                let result = (|| -> anyhow::Result<()> {
                    for entry in git::list_worktrees(&repo)?.into_iter().skip(1).filter(|entry| entry.trusted) {
                        if owned.iter().any(|path| paths::same_location(path, &entry.path))
                            || atomics_core::identity::is_managed_worktree(&entry.path)
                        {
                            atomics_core::identity::sync_worktree_identity(&entry.path, &identity)?;
                        }
                    }
                    Ok(())
                })();
                if result.is_err() {
                    let _ = actions.send(Box::new(|state| {
                        state.toast("Could not update a worktree identity; integrate will check again.")
                    }));
                }
            }
        });
    }

    fn project_root_of(&self, repo: &Path) -> PathBuf {
        self.project_roots
            .get(repo)
            .cloned()
            .or_else(|| self.project_of(repo))
            .or_else(|| workspace::project_root(repo).ok())
            .unwrap_or_else(|| repo.to_path_buf())
    }

    pub(super) fn plan_base_ref(&self, plan: &Plan) -> String {
        let root =
            plan.project.clone().or_else(|| plan.tasks.iter().find_map(|t| t.repo.as_deref().map(paths::expand)));
        root.map(|root| self.project_base_ref(&root)).unwrap_or_else(|| self.config.base_ref.clone())
    }

    pub(super) fn plan_workflow(&self, plan: &Plan) -> atomics_core::workflow::Workflow {
        let root =
            plan.project.clone().or_else(|| plan.tasks.iter().find_map(|t| t.repo.as_deref().map(paths::expand)));
        match root.as_deref() {
            Some(root) => match self.cached_repo_workflow(root) {
                Some(repo) => repo
                    .config
                    .table
                    .get("workflow")
                    .and_then(|value| value.clone().try_into().ok())
                    .unwrap_or_else(|| self.workflow().clone()),
                None => self.project_config(root).workflow,
            },
            None => self.workflow().clone(),
        }
    }

    pub(super) fn reload_repo_workflows(&mut self) -> bool {
        let mut roots = self.repos.clone();
        roots.extend(self.plans.iter().filter_map(|p| p.project.clone()));
        roots.extend(
            self.plans.iter().flat_map(|p| p.tasks.iter()).filter_map(|t| t.repo.as_deref().map(paths::expand)),
        );
        // Agents' folders too, so their diffs find their project's settings in the cache.
        roots.extend(self.diffs.keys().cloned());
        roots.sort();
        roots.dedup();
        let mut roots: Vec<_> = roots
            .into_iter()
            .map(|root| if self.repo_workflows.contains_key(&root) { root } else { self.resolve_project_root(&root) })
            .collect();
        roots.sort();
        roots.dedup();
        let mut changed = false;
        self.repo_workflows.retain(|root, _| roots.contains(root));
        for root in roots {
            let loaded = RepoWorkflow::read(&root);
            if self.repo_workflows.get(&root).is_none_or(|old| old.fingerprint != loaded.fingerprint) {
                self.repo_workflows.insert(root, loaded);
                for diff in self.diffs.values_mut() {
                    diff.mark_dirty();
                }
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, with_app};
    use super::*;

    #[test]
    fn review_p7_diff_prefers_a_live_session_in_an_open_plan_over_reused_worktrees() {
        with_app(
            "app::repo_workflow::tests::review_p7_diff_prefers_a_live_session_in_an_open_plan_over_reused_worktrees",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let root = dir.path().to_path_buf();
                let task = |session: &str, start: &str| Task {
                    session: Some(session.into()),
                    worktree: Some(root.clone()),
                    start_commit: Some(start.into()),
                    ..Task::default()
                };
                app.runtime.plans = vec![
                    Plan {
                        id: "archived".into(),
                        status: plan::PlanStatus::Archived,
                        tasks: vec![task("old", "old-start")],
                        ..Plan::default()
                    },
                    Plan { id: "unattached".into(), tasks: vec![task("gone", "orphan-start")], ..Plan::default() },
                    Plan { id: "current".into(), tasks: vec![task("live", "current-start")], ..Plan::default() },
                ];
                app.runtime.sessions = vec![
                    super::super::tests::session("old", AgentKind::Generic, AgentStatus::Exited, &root),
                    super::super::tests::session("live", AgentKind::Generic, AgentStatus::Idle, &root),
                ];
                assert_eq!(app.runtime.diff_base_ref(&root), "current-start");
                app.runtime.plans.reverse();
                assert_eq!(app.runtime.diff_base_ref(&root), "current-start");
            },
        );
    }

    #[test]
    fn audit_p7_stacked_task_diffs_start_at_dispatch_for_the_board_and_control_socket() {
        with_app(
            "app::repo_workflow::tests::audit_p7_stacked_task_diffs_start_at_dispatch_for_the_board_and_control_socket",
            |app, _| {
                let (_dir, repo) = repository();
                git::git(&repo, &["switch", "-qc", "integration"]).unwrap();
                std::fs::write(repo.join("dependency"), "dependency change\n").unwrap();
                git::git(&repo, &["add", "dependency"]).unwrap();
                git::git(&repo, &["commit", "-qm", "Dependency"]).unwrap();
                let start = git::commit_of(&repo, "HEAD").unwrap().unwrap();
                let worktree = repo.parent().unwrap().join("stacked");
                git::add_worktree(&repo, &worktree, "stacked-task", "integration").unwrap();
                let worktree = worktree.canonicalize().unwrap();
                std::fs::write(worktree.join("own-commit"), "committed change\n").unwrap();
                git::git(&worktree, &["add", "own-commit"]).unwrap();
                git::git(&worktree, &["commit", "-qm", "Task change"]).unwrap();
                std::fs::write(worktree.join("own-commit"), "also unstaged\n").unwrap();
                std::fs::write(worktree.join("own-untracked"), "untracked change\n").unwrap();
                app.runtime.plans = vec![Plan {
                    tasks: vec![Task {
                        id: "stacked".into(),
                        session: Some("stacked-session".into()),
                        worktree: Some(worktree.clone()),
                        start_commit: Some(start.clone()),
                        ..Task::default()
                    }],
                    ..Plan::default()
                }];
                app.runtime.sessions = vec![super::super::tests::session(
                    "stacked-session",
                    AgentKind::Generic,
                    AgentStatus::Idle,
                    &worktree,
                )];
                app.runtime.config.base_ref = "main".into();
                assert_eq!(app.runtime.diff_base_ref(&worktree), start);
                assert_eq!(app.runtime.diff_base_ref(&repo), "main");
                let (tx, rx) = std::sync::mpsc::channel();
                app.runtime
                    .control(ControlRequest::Diff { session: "stacked-session".into(), patch: true, path: None }, tx);
                let ControlOutcome::Diff { base_ref, root, .. } = rx.recv().unwrap() else {
                    panic!("expected diff work")
                };
                assert_eq!(base_ref, start);
                let diff = git::diff_against_base(&root, &base_ref, None).unwrap();
                assert_eq!(diff.base, start);
                assert_eq!(
                    diff.files.iter().map(|file| file.path()).collect::<Vec<_>>(),
                    ["own-commit", "own-untracked"]
                );
                // A rewritten task branch still compares to the exact starting commit.
                git::git(&worktree, &["reset", "--hard", "main"]).unwrap();
                let diff = git::diff_against_base(&root, &base_ref, None).unwrap();
                assert_eq!(diff.base, start);
                assert!(diff.files.iter().any(|file| file.path() == "dependency"));
            },
        );
    }

    #[test]
    fn repo_settings_override_workspace_and_gate_only_their_project() {
        with_app(
            "app::repo_workflow::tests::repo_settings_override_workspace_and_gate_only_their_project",
            |app, _| {
                let (_dir, repo) = repository();
                let repo = repo.canonicalize().unwrap();
                app.runtime.repos = vec![repo.clone()];
                app.runtime.config.tickets.branch = Some("workspace/{slug}".into());
                std::fs::create_dir(repo.join(".atomics")).unwrap();
                std::fs::write(repo.join(".atomics/config.toml"), "base_ref = 'main'\n[tickets]\nbranch = 'issue/{key_number}-{slug}'\n[workflow]\ndone = 'qa_passed'\nstages = ['accepted', 'pr_opened', 'qa_passed']").unwrap();
                app.runtime.reload_repo_workflows();
                let plan = Plan {
                    project: Some(repo.clone()),
                    tasks: vec![plan::Task { id: "a".into(), status: TaskStatus::Done, ..Default::default() }],
                    ..Default::default()
                };
                assert_eq!(app.runtime.plan_workflow(&plan).done, "qa_passed");
                assert_eq!(app.runtime.plan_workflow(&Plan::default()).done, "accepted");
                let config = app.runtime.project_config(&repo);
                assert_eq!(config.base_ref, "main");
                let ticket = atomics_core::tickets::TicketLink {
                    id: "ENG-1234".into(),
                    key: Some("ENG-1234".into()),
                    title: Some("Project policy".into()),
                    ..Default::default()
                };
                assert_eq!(config.tickets.branch_name(&ticket).as_deref(), Some("issue/1234-project-policy"));
                let mut plan = plan;
                plan.id = "repo-gate".into();
                app.runtime.plans.push(plan);
                let index = app.runtime.plans.len() - 1;
                assert!(app.runtime.close_plan(index, PlanStatus::Completed).is_err());
                let author = atomics_core::tickets::Author::Chief("GPT-6".into());
                app.runtime
                    .record_stage(
                        index,
                        None,
                        atomics_core::workflow::Stage { stage: "qa_passed".into(), ..Default::default() },
                        author,
                    )
                    .unwrap();
                assert_eq!(app.runtime.plans[index].status, PlanStatus::Completed);
                std::fs::remove_file(repo.join(".atomics/config.toml")).unwrap();
                app.runtime.reload_repo_workflows();
                assert_eq!(app.runtime.project_config(&repo).tickets.branch.as_deref(), Some("workspace/{slug}"));
            },
        );
    }

    #[test]
    fn diff_base_refs_of_agent_folders_come_from_the_cache_without_git() {
        with_app(
            "app::repo_workflow::tests::diff_base_refs_of_agent_folders_come_from_the_cache_without_git",
            |app, _ctx| {
                let (_dir, repo) = repository();
                let repo = repo.canonicalize().unwrap();
                std::fs::create_dir(repo.join(".atomics")).unwrap();
                std::fs::write(repo.join(".atomics/config.toml"), "base_ref = 'develop'\n").unwrap();
                let linked = repo.parent().unwrap().join("agent");
                git::add_worktree(&repo, &linked, "agent-task", "main").unwrap();
                let linked = linked.canonicalize().unwrap();
                // An agent folder outside the open projects, as when a workspace's chief runs agents elsewhere.
                app.runtime.diffs.entry(linked.clone()).or_default();
                app.runtime.runtime_tick();
                app.runtime.reload_repo_workflows();
                assert_eq!(app.runtime.project_roots[&linked], repo);
                assert_eq!(app.runtime.project_base_ref(&linked), "develop");
                // Every later tick answers from the cache: with git unusable the base ref is unchanged.
                std::fs::rename(repo.join(".git"), repo.join(".git-moved")).unwrap();
                app.runtime.runtime_tick();
                assert_eq!(app.runtime.project_base_ref(&linked), "develop");
                assert_eq!(app.runtime.project_config(&linked).base_ref, "develop");
                std::fs::rename(repo.join(".git-moved"), repo.join(".git")).unwrap();
            },
        );
    }

    #[test]
    fn project_instructions_refresh_from_main_checkout() {
        with_app("app::repo_workflow::tests::project_instructions_refresh_from_main_checkout", |app, _| {
            let (_dir, repo) = repository();
            let repo = repo.canonicalize().unwrap();
            app.runtime.repos = vec![repo.clone()];
            std::fs::write(repo.join("ATOMICS.md"), "Main instructions A").unwrap();
            assert!(app.runtime.reload_repo_workflows());
            assert!(app.runtime.chief_prompt().contains("Main instructions A"));
            assert!(!app.runtime.reload_repo_workflows());
            let file = repo.join("ATOMICS.md");
            std::fs::write(&file, "Main instructions B").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                .unwrap();
            assert!(app.runtime.reload_repo_workflows());
            assert!(
                app.runtime.repo_workflows[&workspace::project_root(&repo).unwrap()]
                    .agent_section()
                    .contains("Main instructions B")
            );
            assert!(app.runtime.chief_prompt().contains("Main instructions B"));
            let linked = repo.parent().unwrap().join("linked");
            git::add_worktree(&repo, &linked, "instructions-test", "main").unwrap();
            std::fs::write(linked.join("ATOMICS.md"), "Worktree instructions").unwrap();
            let main = workspace::project_root(&linked).unwrap();
            assert!(RepoWorkflow::read(&main).agent_section().contains("Main instructions B"));
            assert!(!RepoWorkflow::read(&main).agent_section().contains("Worktree instructions"));
            std::fs::remove_file(file).unwrap();
            assert!(app.runtime.reload_repo_workflows());
            assert!(!app.runtime.chief_prompt().contains("Main instructions B"));
        });
    }
}
