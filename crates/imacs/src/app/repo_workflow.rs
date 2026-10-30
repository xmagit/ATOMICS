use super::*;
use imacs_core::repo_workflow::RepoWorkflow;

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

    pub(super) fn project_config(&self, repo: &Path) -> Config {
        if let Some(workflow) = self.cached_repo_workflow(repo) {
            return workflow.apply(&self.effective_config());
        }
        let root = self
            .project_roots
            .get(repo)
            .cloned()
            .or_else(|| self.project_of(repo))
            .or_else(|| workspace::project_root(repo).ok())
            .unwrap_or_else(|| repo.to_path_buf());
        let config = self.effective_config();
        match self.repo_workflows.get(&root) {
            Some(workflow) => workflow.apply(&config),
            None => RepoWorkflow::read(&root).apply(&config),
        }
    }

    pub(super) fn plan_base_ref(&self, plan: &Plan) -> String {
        let root =
            plan.project.clone().or_else(|| plan.tasks.iter().find_map(|t| t.repo.as_deref().map(paths::expand)));
        root.map(|root| self.project_base_ref(&root)).unwrap_or_else(|| self.config.base_ref.clone())
    }

    pub(super) fn plan_workflow(&self, plan: &Plan) -> imacs_core::workflow::Workflow {
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
    fn repo_settings_override_workspace_and_gate_only_their_project() {
        with_app(
            "app::repo_workflow::tests::repo_settings_override_workspace_and_gate_only_their_project",
            |app, _| {
                let (_dir, repo) = repository();
                let repo = repo.canonicalize().unwrap();
                app.runtime.repos = vec![repo.clone()];
                app.runtime.config.tickets.branch = Some("workspace/{slug}".into());
                std::fs::create_dir(repo.join(".imacs")).unwrap();
                std::fs::write(repo.join(".imacs/config.toml"), "base_ref = 'main'\n[tickets]\nbranch = 'issue/{key_number}-{slug}'\n[workflow]\ndone = 'qa_passed'\nstages = ['accepted', 'pr_opened', 'qa_passed']").unwrap();
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
                let ticket = imacs_core::tickets::TicketLink {
                    id: "NEO-1234".into(),
                    key: Some("NEO-1234".into()),
                    title: Some("Project policy".into()),
                    ..Default::default()
                };
                assert_eq!(config.tickets.branch_name(&ticket).as_deref(), Some("issue/1234-project-policy"));
                let mut plan = plan;
                plan.id = "repo-gate".into();
                app.runtime.plans.push(plan);
                let index = app.runtime.plans.len() - 1;
                assert!(app.runtime.close_plan(index, PlanStatus::Completed).is_err());
                let author = imacs_core::tickets::Author::Chief("GPT-6".into());
                app.runtime
                    .record_stage(
                        index,
                        None,
                        imacs_core::workflow::Stage { stage: "qa_passed".into(), ..Default::default() },
                        author,
                    )
                    .unwrap();
                assert_eq!(app.runtime.plans[index].status, PlanStatus::Completed);
                std::fs::remove_file(repo.join(".imacs/config.toml")).unwrap();
                app.runtime.reload_repo_workflows();
                assert_eq!(app.runtime.project_config(&repo).tickets.branch.as_deref(), Some("workspace/{slug}"));
            },
        );
    }

    #[test]
    fn diff_base_refs_of_agent_folders_come_from_the_cache_without_git() {
        with_app(
            "app::repo_workflow::tests::diff_base_refs_of_agent_folders_come_from_the_cache_without_git",
            |app, ctx| {
                let (_dir, repo) = repository();
                let repo = repo.canonicalize().unwrap();
                std::fs::create_dir(repo.join(".imacs")).unwrap();
                std::fs::write(repo.join(".imacs/config.toml"), "base_ref = 'develop'\n").unwrap();
                let linked = repo.parent().unwrap().join("agent");
                git::add_worktree(&repo, &linked, "agent-task", "main").unwrap();
                let linked = linked.canonicalize().unwrap();
                // An agent folder outside the open projects, as when a workspace's chief runs agents elsewhere.
                app.runtime.diffs.entry(linked.clone()).or_default();
                app.runtime.runtime_tick(ctx);
                app.runtime.reload_repo_workflows();
                assert_eq!(app.runtime.project_roots[&linked], repo);
                assert_eq!(app.runtime.project_base_ref(&linked), "develop");
                // Every later tick answers from the cache: with git unusable the base ref is unchanged.
                std::fs::rename(repo.join(".git"), repo.join(".git-moved")).unwrap();
                app.runtime.runtime_tick(ctx);
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
            std::fs::write(repo.join("IMACS.md"), "Main instructions A").unwrap();
            assert!(app.runtime.reload_repo_workflows());
            assert!(app.runtime.chief_prompt().contains("Main instructions A"));
            assert!(!app.runtime.reload_repo_workflows());
            let file = repo.join("IMACS.md");
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
            std::fs::write(linked.join("IMACS.md"), "Worktree instructions").unwrap();
            let main = workspace::project_root(&linked).unwrap();
            assert!(RepoWorkflow::read(&main).agent_section().contains("Main instructions B"));
            assert!(!RepoWorkflow::read(&main).agent_section().contains("Worktree instructions"));
            std::fs::remove_file(file).unwrap();
            assert!(app.runtime.reload_repo_workflows());
            assert!(!app.runtime.chief_prompt().contains("Main instructions B"));
        });
    }
}
