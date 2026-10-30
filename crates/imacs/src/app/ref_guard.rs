use super::*;
use crate::ref_guard::Exempt;

impl State {
    /// Branches whose moves are legitimate work: those of live agents, unfinished tasks and
    /// worktrees being made. A done task's branch is finished, and moving it is restored. Runs no
    /// git: [`Exempt::resolve`] does that on the ref guard thread.
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
                let _ = actions.send(Box::new(move |state| state.ref_alert(&id, alerts.join("\n"))));
            }
        });
    }

    pub(super) fn ref_alert(&mut self, id: &str, details: String) {
        let summary = format!("SANDBOX REF GUARD: {details}");
        if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions[index].last_message = Some(summary.clone());
        }
        let mut owners = Vec::new();
        for (index, plan) in self.plans.iter_mut().enumerate() {
            for task in plan.tasks.iter_mut().filter(|task| task.session.as_deref() == Some(id)) {
                task.ref_alert = Some(summary.clone());
                task.notes.push(summary.clone());
                owners.push(index);
            }
        }
        for index in owners {
            self.changed_plans.insert(self.plans[index].id.clone());
            self.save_plan(index);
        }
        // Reports wake the chief's normal imacs_wait loop, even after the tile was closed.
        self.report(Some(id), "blocked", &summary);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, session, with_app};
    use super::*;

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

                let deadline = Instant::now() + Duration::from_secs(10);
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
}
