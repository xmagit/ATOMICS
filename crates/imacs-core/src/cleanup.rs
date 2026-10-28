//! Preview and safely remove only a completed plan's recorded branches and worktrees.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail, ensure};

use crate::git;
use crate::plan::{Plan, PlanStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub repo: PathBuf,
    pub branch: String,
    pub worktree: Option<PathBuf>,
    pub keep: Option<String>,
    tip: String,
}

pub fn preview(plan: &Plan, base_ref: &str) -> Result<Vec<Entry>> {
    ensure!(plan.status == PlanStatus::Completed, "Only completed plans can be cleaned up");
    let mut repos: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for task in &plan.tasks {
        let Some(branch) = &task.branch else { continue };
        let repo = task.repo.as_deref().map(crate::paths::expand).or_else(|| plan.project.clone());
        let repo = repo.ok_or_else(|| anyhow::anyhow!("No recorded repository for task {}", task.id))?;
        let main = git::list_worktrees(&repo)?.into_iter().next().ok_or_else(|| anyhow::anyhow!("No main checkout"))?;
        repos.entry(main.path).or_default().insert(branch.clone());
    }
    if repos.is_empty()
        && let Some(repo) = &plan.project
    {
        let main = git::list_worktrees(repo)?.into_iter().next().ok_or_else(|| anyhow::anyhow!("No main checkout"))?;
        repos.entry(main.path).or_default();
    }
    let integration = plan.integration_branch();
    let mut entries = Vec::new();
    for (repo, mut branches) in repos {
        let base = git::resolve_base(&repo, base_ref)?;
        let exists = |branch: &str| {
            git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}^{{commit}}")]).is_ok()
        };
        let merged = |branch: &str| {
            git::git(&repo, &["merge-base", "--is-ancestor", &format!("refs/heads/{branch}"), &base]).is_ok()
        };
        let finished = (exists(&integration) && merged(&integration))
            || (!branches.is_empty() && branches.iter().all(|branch| !exists(branch) || merged(branch)));
        branches.insert(integration.clone());
        let worktrees = git::list_worktrees(&repo)?;
        let health = git::worktree_health(&repo, &base)?;
        let mut branches: Vec<_> = branches.into_iter().filter(|branch| exists(branch)).collect();
        // Keep the integration branch available until all task removals have rechecked it.
        branches.sort_by_key(|branch| branch == &integration);
        for branch in branches {
            let worktree = worktrees.iter().find(|w| w.branch.as_ref() == Some(&branch)).map(|w| w.path.clone());
            let h = health.iter().find(|h| h.branch.as_ref() == Some(&branch));
            let keep = if git::check_branch_name(&repo, &branch, &base).is_err() || worktree.as_ref() == Some(&repo) {
                Some("main checkout or protected branch".into())
            } else if !finished {
                Some(format!("plan has not been merged into {base}"))
            } else if !merged(&branch) {
                Some(format!("unmerged commits: branch is not contained in {base}"))
            } else if let Some(h) = h {
                if h.missing {
                    Some("worktree folder is missing".into())
                } else {
                    // Unlike the general explorer, failed status checks must fail closed here.
                    let status = git::git(&h.path, &["status", "--porcelain", "--untracked-files=all"])?;
                    (!status.is_empty()).then(|| "uncommitted changes".into())
                }
            } else {
                None
            };
            let tip = git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")])?.trim().to_string();
            entries.push(Entry { repo: repo.clone(), branch, worktree, keep, tip });
        }
    }
    Ok(entries)
}

pub fn remove(plan: &Plan, base_ref: &str, entry: &Entry) -> Result<()> {
    let current = preview(plan, base_ref)?
        .into_iter()
        .find(|row| row.repo == entry.repo && row.branch == entry.branch)
        .ok_or_else(|| anyhow::anyhow!("branch no longer belongs to this preview"))?;
    if let Some(reason) = &current.keep {
        bail!("{reason}");
    }
    ensure!(current == *entry, "branch or worktree changed since preview; reload before removing");
    if let Some(path) = &entry.worktree {
        git::remove_worktree(&entry.repo, path, None, false)?;
    }
    // Containment was checked against base, independently of HEAD or the branch's upstream.
    git::git(&entry.repo, &["update-ref", "-d", &format!("refs/heads/{}", entry.branch), &entry.tip]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Task;

    fn plan(repo: &std::path::Path, branches: &[&str]) -> Plan {
        Plan {
            id: "ship".into(),
            status: PlanStatus::Completed,
            project: Some(repo.into()),
            tasks: branches.iter().map(|b| Task { branch: Some((*b).into()), ..Task::default() }).collect(),
            ..Plan::default()
        }
    }

    fn worktree(repo: &std::path::Path, branch: &str) -> PathBuf {
        let path = repo.with_file_name(branch.replace('/', "-"));
        git::add_worktree(repo, &path, branch, "main").unwrap();
        path
    }

    fn commit(repo: &std::path::Path, file: &str) {
        std::fs::write(repo.join(file), file).unwrap();
        git::commit_all(repo, file).unwrap();
    }

    #[test]
    fn merged_completed_plan_removes_exactly_its_own_branches_and_worktrees() {
        let (_dir, repo) = git::tests::project();
        git::git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        let task = worktree(&repo, "task");
        let other = worktree(&repo, "another-plan");
        let into = worktree(&repo, "imacs/plan-ship");
        commit(&task, "feature");
        git::merge(&into, "task").unwrap();
        git::merge(&repo, "imacs/plan-ship").unwrap();
        git::git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        let plan = plan(&repo, &["task", "task"]);
        let rows = preview(&plan, "origin/main").unwrap();
        assert_eq!(rows.iter().map(|r| r.branch.as_str()).collect::<Vec<_>>(), ["task", "imacs/plan-ship"]);
        assert!(rows.iter().all(|r| r.keep.is_none()));
        assert!(task.exists() && into.exists(), "preview never removes anything");
        for row in rows {
            remove(&plan, "origin/main", &row).unwrap();
        }
        assert!(!task.exists() && !into.exists());
        assert!(other.exists());
        assert_eq!(git::git(&repo, &["branch", "--format=%(refname:short)"]).unwrap(), "another-plan\nmain\n");
        assert!(preview(&plan, "main").unwrap().is_empty());
    }

    #[test]
    fn cleanup_uses_remote_base_even_when_local_main_has_a_different_tip() {
        let (_dir, repo) = git::tests::project();
        let task = worktree(&repo, "task");
        commit(&task, "feature");
        git::git(&repo, &["remote", "add", "origin", task.to_str().unwrap()]).unwrap();
        git::git(&repo, &["fetch", "origin", "task:refs/remotes/origin/main"]).unwrap();
        let plan = plan(&repo, &["task"]);
        assert!(preview(&plan, "main").unwrap()[0].keep.is_some(), "local main is still behind");
        let row = preview(&plan, "origin/main").unwrap().remove(0);
        assert!(row.keep.is_none(), "fetched remote main contains the task");
        remove(&plan, "origin/main", &row).unwrap();
        assert!(!task.exists());
        assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/task"]).is_err());

        let task = worktree(&repo, "local-only");
        commit(&task, "unpublished");
        git::merge(&repo, "local-only").unwrap();
        let plan = self::plan(&repo, &["local-only"]);
        assert!(preview(&plan, "main").unwrap()[0].keep.is_none());
        let row = preview(&plan, "origin/main").unwrap().remove(0);
        assert!(row.keep.is_some(), "local main must not authorize deletion of unpublished work");
        assert!(remove(&plan, "origin/main", &row).is_err());
        assert!(task.join("unpublished").exists());
    }

    #[test]
    fn merged_branch_cleanup_does_not_depend_on_head_or_upstream() {
        for has_worktree in [false, true] {
            for stale_upstream in [false, true] {
                let (_dir, repo) = git::tests::project();
                git::git(&repo, &["branch", "unrelated"]).unwrap();
                let task = worktree(&repo, "task");
                commit(&task, "feature");
                git::merge(&repo, "task").unwrap();
                if stale_upstream {
                    git::git(&repo, &["branch", "--set-upstream-to=unrelated", "task"]).unwrap();
                } else {
                    git::git(&repo, &["checkout", "unrelated"]).unwrap();
                }
                if !has_worktree {
                    git::remove_worktree(&repo, &task, None, false).unwrap();
                }
                let plan = plan(&repo, &["task"]);
                let row = preview(&plan, "main").unwrap().remove(0);
                assert!(row.keep.is_none());
                remove(&plan, "main", &row).unwrap();
                assert!(!task.exists());
                assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/task"]).is_err());
                assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/unrelated"]).is_ok());
            }
        }
    }

    #[test]
    fn dirty_work_is_kept_and_explained_in_real_worktrees() {
        let (_dir, repo) = git::tests::project();
        commit(&repo, "tracked-file");
        let plan = plan(&repo, &["task"]);
        let task = worktree(&repo, "task");
        let clean = preview(&plan, "main").unwrap().remove(0);
        std::fs::write(task.join("tracked-file"), "keep tracked edits").unwrap();
        assert_eq!(preview(&plan, "main").unwrap()[0].keep.as_deref(), Some("uncommitted changes"));
        assert!(remove(&plan, "main", &clean).is_err());
        assert_eq!(std::fs::read_to_string(task.join("tracked-file")).unwrap(), "keep tracked edits");
        for staged in [false, true] {
            std::fs::write(task.join("new-file"), "keep me").unwrap();
            if staged {
                git::git(&task, &["add", "new-file"]).unwrap();
            }
            let row = preview(&plan, "main").unwrap().remove(0);
            assert_eq!(row.keep.as_deref(), Some("uncommitted changes"));
            assert!(remove(&plan, "main", &clean).unwrap_err().to_string().contains("uncommitted"));
            assert_eq!(std::fs::read_to_string(task.join("new-file")).unwrap(), "keep me");
        }
    }

    #[test]
    fn unmerged_work_is_kept_even_with_a_deleted_upstream_and_merged_integration() {
        let (_dir, repo) = git::tests::project();
        let task = worktree(&repo, "task");
        let into = worktree(&repo, "imacs/plan-ship");
        git::git(&repo, &["remote", "add", "origin", "https://example.invalid/repo"]).unwrap();
        git::git(&repo, &["config", "branch.task.remote", "origin"]).unwrap();
        git::git(&repo, &["config", "branch.task.merge", "refs/heads/task"]).unwrap();
        commit(&task, "not-merged");
        assert!(
            git::worktree_health(&repo, "main")
                .unwrap()
                .iter()
                .find(|h| h.branch.as_deref() == Some("task"))
                .unwrap()
                .upstream_gone
        );
        let plan = plan(&repo, &["task"]);
        let rows = preview(&plan, "main").unwrap();
        let row = rows.iter().find(|r| r.branch == "task").unwrap();
        assert!(row.keep.as_deref().unwrap().contains("unmerged commits"));
        assert!(remove(&plan, "main", row).is_err());
        remove(&plan, "main", rows.last().unwrap()).unwrap();
        assert!(task.join("not-merged").exists());
        assert!(!into.exists());
    }

    #[test]
    fn main_containing_every_task_allows_cleanup_without_merged_integration() {
        let (_dir, repo) = git::tests::project();
        let task = worktree(&repo, "task");
        commit(&task, "feature");
        git::merge(&repo, "task").unwrap();
        let into = worktree(&repo, "imacs/plan-ship");
        commit(&into, "integration-only");
        let plan = plan(&repo, &["task"]);
        let rows = preview(&plan, "main").unwrap();
        assert!(rows[0].keep.is_none());
        assert!(rows[1].keep.as_deref().unwrap().contains("unmerged"));
        remove(&plan, "main", &rows[0]).unwrap();
        assert!(!task.exists());
        assert!(into.join("integration-only").exists());
    }

    #[test]
    fn incomplete_and_unmerged_plans_cannot_be_cleaned_up() {
        let (_dir, repo) = git::tests::project();
        let task = worktree(&repo, "task");
        commit(&task, "feature");
        worktree(&repo, "imacs/plan-ship");
        let mut plan = plan(&repo, &["task"]);
        // Give the integration branch its own unmerged work too.
        let into = repo.with_file_name("imacs-plan-ship");
        commit(&into, "integration");
        let rows = preview(&plan, "main").unwrap();
        assert!(rows.iter().all(|r| r.keep.as_deref().unwrap().contains("plan has not been merged")));
        for status in [PlanStatus::Open, PlanStatus::Archived] {
            plan.status = status;
            assert!(preview(&plan, "main").is_err());
            assert!(remove(&plan, "main", &rows[0]).is_err());
        }
        assert!(task.exists() && into.exists());
    }

    #[test]
    fn branch_only_cleanup_rechecks_new_commits_and_preview_identity() {
        let (_dir, repo) = git::tests::project();
        git::git(&repo, &["branch", "task"]).unwrap();
        let plan = plan(&repo, &["task"]);
        let row = preview(&plan, "main").unwrap().remove(0);
        assert!(row.worktree.is_none());
        git::git(&repo, &["checkout", "task"]).unwrap();
        commit(&repo, "new-work");
        git::git(&repo, &["checkout", "main"]).unwrap();
        assert!(remove(&plan, "main", &row).is_err());
        git::merge(&repo, "task").unwrap();
        assert!(remove(&plan, "main", &row).unwrap_err().to_string().contains("changed since preview"));
        remove(&plan, "main", &preview(&plan, "main").unwrap()[0]).unwrap();
        assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/task"]).is_err());
    }

    #[test]
    fn protected_and_main_checkout_branches_are_kept() {
        let (_dir, repo) = git::tests::project();
        let plan = plan(&repo, &["main"]);
        let row = preview(&plan, "main").unwrap().remove(0);
        assert_eq!(row.keep.as_deref(), Some("main checkout or protected branch"));
        assert!(remove(&plan, "main", &row).is_err());
        assert!(repo.exists());
    }

    #[test]
    fn repositories_are_scoped_per_task_and_missing_metadata_fails_closed() {
        let (_dir, repo) = git::tests::project();
        let (_other_dir, other) = git::tests::project();
        let one = worktree(&repo, "one");
        let two = worktree(&other, "two");
        let unrelated = worktree(&other, "one");
        let mut plan = plan(&repo, &["one", "two"]);
        plan.tasks[1].repo = Some(other.to_string_lossy().into_owned());
        let rows = preview(&plan, "main").unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            remove(&plan, "main", &row).unwrap();
        }
        assert!(!one.exists() && !two.exists());
        assert!(unrelated.exists());
        plan.project = None;
        assert!(preview(&plan, "main").is_err());
    }

    #[test]
    fn integration_only_plan_can_be_cleaned_up_and_missing_worktrees_are_kept() {
        let (_dir, repo) = git::tests::project();
        let into = worktree(&repo, "imacs/plan-ship");
        let plan = plan(&repo, &[]);
        let row = preview(&plan, "main").unwrap().remove(0);
        std::fs::rename(&into, into.with_extension("moved")).unwrap();
        assert_eq!(preview(&plan, "main").unwrap()[0].keep.as_deref(), Some("worktree folder is missing"));
        assert!(remove(&plan, "main", &row).is_err());
    }

    #[test]
    fn removal_never_forces_a_locked_worktree_or_removes_a_reassigned_one() {
        let (_dir, repo) = git::tests::project();
        let task = worktree(&repo, "task");
        let plan = plan(&repo, &["task"]);
        let row = preview(&plan, "main").unwrap().remove(0);
        git::git(&repo, &["worktree", "lock", task.to_str().unwrap()]).unwrap();
        assert!(remove(&plan, "main", &row).is_err());
        assert!(task.exists());
        assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/task"]).is_ok());
        git::git(&repo, &["worktree", "unlock", task.to_str().unwrap()]).unwrap();
        git::git(&task, &["checkout", "-b", "another-plan"]).unwrap();
        assert!(remove(&plan, "main", &row).unwrap_err().to_string().contains("changed since preview"));
        let fresh = preview(&plan, "main").unwrap().remove(0);
        assert!(fresh.worktree.is_none());
        remove(&plan, "main", &fresh).unwrap();
        assert!(task.exists());
        assert_eq!(git::worktree_of(&task).unwrap().branch.as_deref(), Some("another-plan"));
    }
}
