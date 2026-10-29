use super::*;
use imacs_core::repo_workflow::RepoWorkflow;

impl State {
    pub(super) fn reload_repo_workflows(&mut self) -> bool {
        let mut roots = self.repos.clone();
        roots.extend(self.plans.iter().filter_map(|p| p.project.clone()));
        roots.sort();
        roots.dedup();
        let mut changed = false;
        self.repo_workflows.retain(|root, _| roots.contains(root));
        for root in roots {
            let loaded = RepoWorkflow::read(&root);
            if self.repo_workflows.get(&root).is_none_or(|old| old.fingerprint != loaded.fingerprint) {
                self.repo_workflows.insert(root, loaded);
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
    fn project_instructions_refresh_from_main_checkout() {
        with_app("app::repo_workflow::tests::project_instructions_refresh_from_main_checkout", |app, _| {
            let (_dir, repo) = repository();
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
            assert!(app.runtime.repo_workflows[&repo].agent_section().contains("Main instructions B"));
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
