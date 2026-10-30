use std::collections::BTreeSet;

use super::*;
use crate::ref_guard::RefGuard;

impl State {
    pub(super) fn concurrent_branches(&self, guard: &RefGuard) -> BTreeSet<String> {
        let mut branches = BTreeSet::new();
        for session in &self.sessions {
            if session.chief || !session.is_agent() {
                continue;
            }
            if let Some(other) = &session.ref_guard {
                if other.common == guard.common {
                    branches.extend(other.branch.clone());
                }
            } else if git::common_dir(&session.cwd).is_ok_and(|common| common == guard.common) {
                branches.extend(session.branch().map(|branch| format!("refs/heads/{branch}")));
            }
        }
        for task in self.plans.iter().flat_map(|plan| &plan.tasks) {
            if let (Some(worktree), Some(branch)) = (&task.worktree, &task.branch)
                && git::common_dir(worktree).is_ok_and(|common| common == guard.common)
            {
                branches.insert(format!("refs/heads/{branch}"));
            }
        }
        for (common, branch, _) in &self.creating {
            if *common == guard.common {
                branches.insert(format!("refs/heads/{branch}"));
            }
        }
        branches
    }

    /// Check the completed turn and save a fresh baseline before the next one, including
    /// turns typed directly into a tile. Exit checks also catch an agent too quick to show busy.
    pub(super) fn check_refs(&mut self, index: usize) {
        let Some(mut guard) = self.sessions[index].ref_guard.take() else { return };
        let branches = self.concurrent_branches(&guard);
        let result = guard.check(&branches);
        self.sessions[index].ref_guard = Some(guard);
        let alerts = result.unwrap_or_else(|error| vec![format!("Ref guard check FAILED: {error:#}")]);
        if !alerts.is_empty() {
            let id = self.sessions[index].id.clone();
            self.ref_alert(&id, alerts.join("\n"));
        }
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
