//! Preview and safely remove only a completed plan's recorded branches and worktrees, and decide which of a closed
//! plan's worktrees its close may remove.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail, ensure};

use crate::git;
use crate::plan::{Plan, PlanStatus, REVIEW_TASK, Task};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub repo: PathBuf,
    pub branch: String,
    pub worktree: Option<PathBuf>,
    pub keep: Option<String>,
    /// The task working on it; none for the plan's own integration checkout.
    pub task: Option<String>,
    tip: String,
}

/// Why a closed plan keeps a worktree it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub reason: String,
    /// Nothing uncommitted is in it: only where its commits went is unproven, which the user may accept.
    pub clean: bool,
}

/// Why a worktree with uncommitted or untracked files stays.
pub const UNCOMMITTED: &str = "uncommitted changes";

/// Why a worktree stays: git lists it from files a sandbox can write that do not lead back to it.
const UNTRUSTED: &str = "worktree is not listed as git made it, so ATOMICS does not run git in it";

pub fn preview(plan: &Plan, base_ref: &str) -> Result<Vec<Entry>> {
    ensure!(plan.status == PlanStatus::Completed, "Only completed plans can be cleaned up");
    let mut repos: BTreeMap<PathBuf, Vec<&Task>> = BTreeMap::new();
    for task in &plan.tasks {
        let Some(_) = &task.branch else { continue };
        let repo = task.repo.as_deref().map(crate::paths::expand).or_else(|| plan.project.clone());
        let repo = repo.ok_or_else(|| anyhow::anyhow!("No recorded repository for task {}", task.id))?;
        repos.entry(git::main_checkout(&repo)?).or_default().push(task);
    }
    if repos.is_empty()
        && let Some(repo) = &plan.project
    {
        repos.entry(git::main_checkout(repo)?).or_default();
    }
    let integration = plan.integration_branch();
    let mut entries = Vec::new();
    for (repo, tasks) in repos {
        let base = git::resolve_base(&repo, base_ref)?;
        let exists = |branch: &str| {
            git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}^{{commit}}")]).is_ok()
        };
        let merged_into = |branch: &str, into: &str| {
            git::git(&repo, &["merge-base", "--is-ancestor", &format!("refs/heads/{branch}"), into]).is_ok()
        };
        let mut branches: BTreeSet<_> = tasks.iter().filter_map(|t| t.branch.clone()).collect();
        let finished = (exists(&integration) && merged_into(&integration, &base))
            || (!branches.is_empty()
                && tasks
                    .iter()
                    .filter(|t| t.id != REVIEW_TASK)
                    .filter_map(|t| t.branch.as_deref())
                    .all(|branch| !exists(branch) || merged_into(branch, &base)));
        branches.insert(integration.clone());
        let worktrees = git::list_worktrees(&repo)?;
        let health = git::worktree_health(&repo, &base)?;
        let mut branches: Vec<_> = branches.into_iter().filter(|branch| exists(branch)).collect();
        // Keep the integration branch available until all task removals have rechecked it.
        branches.sort_by_key(|branch| branch == &integration);
        for branch in branches {
            // Review starts from integration and carries no work of its own.
            let review = tasks.iter().any(|t| t.id == REVIEW_TASK && t.branch.as_ref() == Some(&branch));
            let worktree = worktrees.iter().find(|w| w.branch.as_ref() == Some(&branch)).map(|w| w.path.clone());
            let h = health.iter().find(|h| h.branch.as_ref() == Some(&branch));
            let keep = if git::check_branch_name(&repo, &branch, &base).is_err() || worktree.as_ref() == Some(&repo) {
                Some("main checkout or protected branch".into())
            } else if !finished {
                Some(format!("plan has not been merged into {base}"))
            } else if !merged_into(&branch, &base)
                && !(review && merged_into(&branch, &format!("refs/heads/{integration}")))
            {
                Some(format!("unmerged commits: branch is not contained in {base}"))
            } else if let Some(h) = h {
                if h.missing {
                    Some("worktree folder is missing".into())
                } else if h.untrusted {
                    Some(UNTRUSTED.into())
                } else {
                    // Unlike the general explorer, failed status checks must fail closed here.
                    let status = git::git(&h.path, &["status", "--porcelain", "--untracked-files=all"])?;
                    (!status.is_empty()).then(|| "uncommitted changes".into())
                }
            } else {
                None
            };
            let tip = git::git(&repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")])?.trim().to_string();
            entries.push(Entry { repo: repo.clone(), branch, worktree, keep, task: None, tip });
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
    // Delete the checked tip, independently of HEAD or the branch's upstream.
    git::git(&entry.repo, &["update-ref", "-d", &format!("refs/heads/{}", entry.branch), &entry.tip])?;
    let section = format!("branch.{}", entry.branch);
    let keys = git::git(&entry.repo, &["config", "--local", "--list", "--name-only"])?;
    if keys.lines().any(|key| key.rsplit_once('.').is_some_and(|(name, _)| name == section)) {
        git::git(&entry.repo, &["config", "--local", "--remove-section", &section])?;
    }
    Ok(())
}

/// Task cleanup on close keeps every unmerged branch. Archive removes only worktrees. Resolve branches from git
/// rather than trusting saved paths, except for the integration branch: its worktree is the plan's only where the
/// plan recorded it, as a task's or as its integration checkout.
pub fn closed_tasks(plan: &Plan) -> Result<Vec<Entry>> {
    ensure!(plan.status != PlanStatus::Open, "Plan is still open");
    let integration = plan.integration_branch();
    let mut wanted: Vec<(PathBuf, &str, Option<&str>, Option<&std::path::Path>)> = Vec::new();
    for task in &plan.tasks {
        let Some(branch) = &task.branch else { continue };
        let repo = task.repo.as_deref().map(crate::paths::expand).or_else(|| plan.project.clone());
        let repo = repo.ok_or_else(|| anyhow::anyhow!("No repository for task {}", task.id))?;
        let only = (branch == &integration).then_some(task.worktree.as_deref());
        if only == Some(None) {
            continue;
        }
        wanted.push((repo, branch, Some(&task.id), only.flatten()));
    }
    for tree in plan.integration_worktrees.iter().filter(|tree| tree.branch == integration) {
        wanted.push((tree.repo.clone(), &tree.branch, None, Some(&tree.path)));
    }
    let same = |a: &std::path::Path, b: &std::path::Path| {
        a == b || a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b)
    };
    let mut entries: Vec<Entry> = Vec::new();
    for (repo, branch, task, only) in wanted {
        let trees = git::list_worktrees(&repo)?;
        let main = trees.first().ok_or_else(|| anyhow::anyhow!("No main checkout"))?;
        git::check_branch_name(&repo, branch, "__atomics_cleanup__")?;
        let reference = format!("refs/heads/{branch}");
        let Ok(tip) = git::git(&repo, &["rev-parse", "--verify", &reference]) else { continue };
        let worktree = trees.iter().find(|tree| tree.branch.as_deref() == Some(branch));
        if only.is_some_and(|only| !worktree.is_some_and(|tree| same(&tree.path, only))) {
            continue;
        }
        let keep = match worktree {
            Some(tree) if tree.path == main.path => Some("main checkout".into()),
            Some(tree) if !tree.trusted && tree.path.exists() => Some(UNTRUSTED.into()),
            _ => None,
        };
        let entry = Entry {
            repo: main.path.clone(),
            branch: branch.into(),
            worktree: worktree.map(|tree| tree.path.clone()),
            keep,
            task: task.map(str::to_string),
            tip: tip.trim().into(),
        };
        if !entries.iter().any(|known| (&known.repo, &known.branch) == (&entry.repo, &entry.branch)) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// The branch a closed plan's work in `repo` is checked against: `base_ref`, fetched just now where it names a
/// remote, or the usual fallbacks; with why it may be behind the remote.
pub fn default_branch(repo: &std::path::Path, base_ref: &str) -> (Option<String>, Option<String>) {
    let fresh = git::fresh_base(repo, base_ref);
    (git::resolve_base(repo, &fresh.base).ok(), fresh.warning)
}

/// Why the closed `plan` keeps the trusted worktree of `entry`, or none when the worktree is clean and its work
/// is safe without it: ATOMICS integrated it (it is in the integration branch), it is on `default`, or its changes
/// are, by itself or through a branch that contains it and was merged whole, as when the plan's branches were
/// combined into one pull request that was squash-merged. The combining branches tried are the integration branch
/// and the heads of the pull requests the plan recorded. `waive` skips the second part, for a removal the user
/// asked for: the branch keeps its commits either way.
pub fn kept_work(plan: &Plan, entry: &Entry, default: Option<&str>, waive: bool) -> Result<Option<Kept>> {
    let Some(path) = &entry.worktree else { return Ok(None) };
    // A failed status check fails closed.
    if !git::git(path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
        return Ok(Some(Kept { reason: UNCOMMITTED.into(), clean: false }));
    }
    if waive {
        return Ok(None);
    }
    let (repo, tip) = (&entry.repo, entry.tip.as_str());
    let integration = plan.integration_branch();
    let contains = |branch: &str| -> Result<bool> {
        Ok(git::check_branch_name(repo, branch, "__atomics_cleanup__").is_ok()
            && git::branch_ref_exists(repo, branch)?
            && git::is_ancestor(repo, tip, &format!("refs/heads/{branch}"))? == Some(true))
    };
    if entry.branch != integration {
        let task = entry.task.as_ref().and_then(|id| plan.tasks.iter().find(|task| &task.id == id));
        if task.is_some_and(|task| task.integrated.is_some() && task.integrated_commit.as_deref() == Some(tip))
            || contains(&integration)?
        {
            return Ok(None);
        }
    }
    let Some(default) = default else {
        return Ok(Some(Kept { reason: "no default branch to check its commits against".into(), clean: true }));
    };
    let landed = |rev: &str| -> Result<bool> {
        Ok(git::is_ancestor(repo, rev, default)? == Some(true) || git::changes_contained(repo, rev, default)?)
    };
    if landed(tip)? {
        return Ok(None);
    }
    let mut combined: Vec<&str> = plan
        .tasks
        .iter()
        .filter_map(|task| task.pull_request.as_ref())
        .chain(&plan.pull_request)
        .map(|pr| pr.head.as_str())
        .collect();
    combined.push(&integration);
    combined.sort_unstable();
    combined.dedup();
    for branch in combined.into_iter().filter(|branch| *branch != entry.branch) {
        if contains(branch)? && landed(&format!("refs/heads/{branch}"))? {
            return Ok(None);
        }
    }
    let shown =
        default.strip_prefix("refs/remotes/").or_else(|| default.strip_prefix("refs/heads/")).unwrap_or(default);
    let recorded = plan.pull_request.is_some() || plan.outputs().iter().any(|output| output.pull_request().is_some());
    let reason = if recorded {
        format!("no merge seen: its commits are not on {shown}")
    } else {
        format!("commits not on {shown}")
    };
    Ok(Some(Kept { reason, clean: true }))
}

/// Returns whether the branch was deleted. A successful worktree removal may retain its branch.
pub fn remove_closed_task(plan: &Plan, entry: &Entry) -> Result<bool> {
    let current = closed_tasks(plan)?
        .into_iter()
        .find(|row| row.repo == entry.repo && row.branch == entry.branch)
        .ok_or_else(|| anyhow::anyhow!("Task branch no longer exists"))?;
    ensure!(current == *entry, "branch or worktree changed since preview");
    if let Some(keep) = &entry.keep {
        bail!("{}", if keep == UNTRUSTED { keep } else { "main checkout is protected" });
    }
    if let Some(path) = &entry.worktree {
        ensure!(
            git::git(path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty(),
            "uncommitted changes in {}",
            path.display()
        );
        git::remove_worktree(&entry.repo, path, None, false)?;
    }
    // The integration branch itself is never deleted here.
    if plan.status == PlanStatus::Archived || entry.branch == plan.integration_branch() {
        return Ok(false);
    }
    let integration = format!("refs/heads/{}", plan.integration_branch());
    if git::git(&entry.repo, &["merge-base", "--is-ancestor", &format!("refs/heads/{}", entry.branch), &integration])
        .is_err()
    {
        return Ok(false);
    }
    // Give branch -d the integration branch as its temporary upstream. Never force deletion,
    // and never depend on the user's current checkout or the task's original upstream.
    git::git(
        &entry.repo,
        &[
            "-c",
            &format!("branch.{}.remote=.", entry.branch),
            "-c",
            &format!("branch.{}.merge={integration}", entry.branch),
            "branch",
            "-d",
            "--",
            &entry.branch,
        ],
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn closing_plans_removes_clean_tasks_deletes_only_merged_branches_and_archive_keeps_branches() {
        for status in [PlanStatus::Completed, PlanStatus::Archived] {
            let (_dir, repo) = git::tests::project();
            let mut plan = plan(&repo, &["merged", "unmerged", "dirty"]);
            plan.status = status;
            let merged = worktree(&repo, "merged");
            let unmerged = worktree(&repo, "unmerged");
            let dirty = worktree(&repo, "dirty");
            let integration = worktree(&repo, &plan.integration_branch());
            commit(&merged, "merged-file");
            git::merge(&integration, "merged").unwrap();
            commit(&unmerged, "unmerged-file");
            std::fs::write(dirty.join("keep"), "dirty").unwrap();
            for entry in closed_tasks(&plan).unwrap() {
                let result = remove_closed_task(&plan, &entry);
                if entry.branch == "dirty" {
                    assert!(result.is_err());
                } else {
                    assert_eq!(result.unwrap(), status == PlanStatus::Completed && entry.branch == "merged");
                }
            }
            assert!(!merged.exists() && !unmerged.exists());
            assert!(dirty.join("keep").exists() && integration.exists());
            let branches = git::git(&repo, &["branch", "--format=%(refname:short)"]).unwrap();
            assert_eq!(branches.lines().any(|b| b == "merged"), status == PlanStatus::Archived);
            assert!(branches.lines().any(|b| b == "unmerged"));
        }
    }

    #[test]
    fn closing_plan_cleanup_rechecks_worktree_identity_and_new_dirt() {
        let (_dir, repo) = git::tests::project();
        let plan = plan(&repo, &["task"]);
        let path = worktree(&repo, "task");
        let entry = closed_tasks(&plan).unwrap().remove(0);
        std::fs::write(path.join("new-work"), "keep").unwrap();
        assert!(remove_closed_task(&plan, &entry).is_err());
        std::fs::remove_file(path.join("new-work")).unwrap();
        git::git(&path, &["checkout", "-b", "other"]).unwrap();
        assert!(remove_closed_task(&plan, &entry).is_err());
        assert!(path.exists());
    }

    #[test]
    fn branch_config_cleanup_is_independent_of_git_language() {
        use std::process::Command;

        const CHILD: &str = "ATOMICS_CLEANUP_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|path| path.join("git"))
                .find(|path| path.is_file())
                .unwrap();
            let wrapper = dir.path().join("git");
            crate::test_support::write_program(
                &wrapper,
                r#"#!/bin/sh
if [ "$7" = --remove-section ]; then
    "$ATOMICS_TEST_REAL_GIT" "$@" 2>/dev/null
    result=$?
    if [ "$result" -ne 0 ]; then
        printf 'fatal: Abschnitt nicht vorhanden\n' >&2
    fi
    exit "$result"
fi
exec "$ATOMICS_TEST_REAL_GIT" "$@"
"#,
            );
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "cleanup::tests::branch_config_cleanup_is_independent_of_git_language",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("ATOMICS_TEST_REAL_GIT", real_git)
                    .env(
                        "PATH",
                        std::env::join_paths(
                            std::iter::once(dir.path().to_path_buf())
                                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                        )
                        .unwrap(),
                    ),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        for configured in [false, true] {
            let (_dir, repo) = git::tests::project();
            git::git(&repo, &["branch", "task"]).unwrap();
            // A different subsection whose name starts with the task's name must survive.
            git::git(&repo, &["config", "branch.task.other.remote", "keep"]).unwrap();
            if configured {
                git::git(&repo, &["config", "branch.task.remote", "origin"]).unwrap();
            }
            let plan = plan(&repo, &["task"]);
            let row = preview(&plan, "main").unwrap().remove(0);
            remove(&plan, "main", &row).unwrap();
            assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/task"]).is_err());
            assert!(git::git(&repo, &["config", "--get", "branch.task.remote"]).is_err());
            assert_eq!(git::git(&repo, &["config", "--get", "branch.task.other.remote"]).unwrap().trim(), "keep");
        }
    }

    #[test]
    fn merged_completed_plan_removes_exactly_its_own_branches_and_worktrees() {
        let (_dir, repo) = git::tests::project();
        git::git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        let task = worktree(&repo, "task");
        let other = worktree(&repo, "another-plan");
        let into = worktree(&repo, "atomics/plan-ship");
        commit(&task, "feature");
        git::merge(&into, "task").unwrap();
        git::merge(&repo, "atomics/plan-ship").unwrap();
        git::git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]).unwrap();
        let plan = plan(&repo, &["task", "task"]);
        let rows = preview(&plan, "origin/main").unwrap();
        assert_eq!(rows.iter().map(|r| r.branch.as_str()).collect::<Vec<_>>(), ["task", "atomics/plan-ship"]);
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
        let into = worktree(&repo, "atomics/plan-ship");
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
        let into = worktree(&repo, "atomics/plan-ship");
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
    fn review_cleanup_uses_base_or_integration_after_each_task_is_merged() {
        let (_dir, repo) = git::tests::project();
        let one = worktree(&repo, "task-one");
        let two = worktree(&repo, "task-two");
        let into = worktree(&repo, "atomics/plan-ship");
        commit(&one, "one");
        commit(&two, "two");
        for branch in ["task-one", "task-two"] {
            git::merge(&into, branch).unwrap();
            git::merge(&repo, branch).unwrap();
        }
        assert!(git::git(&repo, &["merge-base", "--is-ancestor", "atomics/plan-ship", "main"]).is_err());
        // Build the shared history once. Each review still has a real, independent
        // worktree, and both bases exercise clean and unmerged review tips.
        let mut last_plan = None;
        for (index, review_base) in ["main", "atomics/plan-ship"].into_iter().enumerate() {
            for review_has_work in [false, true] {
                let branch = format!("review-{index}-{review_has_work}");
                let review = repo.with_file_name(&branch);
                git::add_worktree(&repo, &review, &branch, review_base).unwrap();
                // Containment in integration also permits an older review tip.
                commit(&into, &format!("integration-{index}-{review_has_work}"));
                if review_has_work {
                    commit(&review, "review-only");
                }
                let mut plan = plan(&repo, &["task-one", "task-two", &branch]);
                plan.tasks[2].id = REVIEW_TASK.into();
                let rows = preview(&plan, "main").unwrap();
                assert_eq!(rows.len(), 4);
                assert!(rows.iter().filter(|row| row.branch.starts_with("task-")).all(|row| row.keep.is_none()));
                let integration = rows.iter().find(|row| row.branch == "atomics/plan-ship").unwrap();
                assert!(integration.keep.as_deref().unwrap().contains("unmerged commits"));
                let row = rows.iter().find(|row| row.branch == branch).unwrap();
                if review_has_work {
                    assert!(row.keep.as_deref().unwrap().contains("unmerged commits"));
                    assert!(remove(&plan, "main", row).is_err());
                } else {
                    assert!(row.keep.is_none());
                    remove(&plan, "main", row).unwrap();
                }
                assert_eq!(review.exists(), review_has_work);
                last_plan = Some(plan);
            }
        }
        // Revalidation after each task removal must still allow the next removal.
        let plan = last_plan.unwrap();
        for row in preview(&plan, "main").unwrap().iter().filter(|row| row.branch.starts_with("task-")) {
            remove(&plan, "main", row).unwrap();
        }
        assert!(!one.exists() && !two.exists());
        assert!(into.join("integration-1-true").exists());
    }

    #[test]
    fn removal_cleans_branch_config_and_allows_missing_sections() {
        for has_worktree in [false, true] {
            for has_config in [false, true] {
                let (_dir, repo) = git::tests::project();
                let task = worktree(&repo, "task/one.v2");
                if !has_worktree {
                    git::remove_worktree(&repo, &task, None, false).unwrap();
                }
                if has_config {
                    git::git(&repo, &["config", "branch.task/one.v2.remote", "."]).unwrap();
                    git::git(&repo, &["config", "branch.task/one.v2.merge", "refs/heads/main"]).unwrap();
                    git::git(&repo, &["config", "branch.task/one.v2.description", "completed task"]).unwrap();
                }
                git::git(&repo, &["config", "branch.other.description", "keep me"]).unwrap();
                let plan = plan(&repo, &["task/one.v2"]);
                let row = preview(&plan, "main").unwrap().remove(0);
                remove(&plan, "main", &row).unwrap();
                assert!(!task.exists());
                assert!(git::git(&repo, &["show-ref", "--verify", "refs/heads/task/one.v2"]).is_err());
                let config = git::git(&repo, &["config", "--local", "--list"]).unwrap();
                assert!(!config.contains("branch.task/one.v2."), "{config}");
                assert!(
                    !std::fs::read_to_string(repo.join(".git/config")).unwrap().contains("[branch \"task/one.v2\"]")
                );
                assert_eq!(git::git(&repo, &["config", "branch.other.description"]).unwrap().trim(), "keep me");
            }
        }
    }

    #[test]
    fn incomplete_and_unmerged_plans_cannot_be_cleaned_up() {
        let (_dir, repo) = git::tests::project();
        let task = worktree(&repo, "task");
        commit(&task, "feature");
        worktree(&repo, "atomics/plan-ship");
        let mut plan = plan(&repo, &["task"]);
        // Give the integration branch its own unmerged work too.
        let into = repo.with_file_name("atomics-plan-ship");
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
        let into = worktree(&repo, "atomics/plan-ship");
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

    /// The entry of `branch` in the closed plan, and why its worktree stays, checked against `default`.
    fn kept(plan: &Plan, branch: &str, default: &str) -> Option<Kept> {
        let entry = closed_tasks(plan).unwrap().into_iter().find(|entry| entry.branch == branch).unwrap();
        kept_work(plan, &entry, Some(default), false).unwrap()
    }

    fn squash(repo: &std::path::Path, branch: &str) {
        git::git(repo, &["merge", "--squash", branch]).unwrap();
        git::git(repo, &["commit", "-qm", &format!("squash {branch}")]).unwrap();
    }

    #[test]
    fn closed_plans_keep_a_worktree_until_its_work_is_on_the_default_branch() {
        let (_dir, repo) = git::tests::project();
        let mut plan = plan(&repo, &["on-main", "dirty", "unmerged", "squashed"]);
        plan.status = PlanStatus::Archived;
        let _on_main = worktree(&repo, "on-main");
        let dirty = worktree(&repo, "dirty");
        let unmerged = worktree(&repo, "unmerged");
        let squashed = worktree(&repo, "squashed");
        std::fs::write(dirty.join("new-work"), "not committed").unwrap();
        commit(&unmerged, "unmerged-file");
        commit(&squashed, "squashed-file");
        squash(&repo, "squashed");
        commit(&repo, "later-on-main");
        assert_eq!(kept(&plan, "on-main", "refs/heads/main"), None, "its head is on main");
        assert_eq!(kept(&plan, "dirty", "refs/heads/main"), Some(Kept { reason: UNCOMMITTED.into(), clean: false }));
        assert_eq!(
            kept(&plan, "unmerged", "refs/heads/main"),
            Some(Kept { reason: "commits not on main".into(), clean: true })
        );
        assert_eq!(kept(&plan, "squashed", "refs/heads/main"), None, "a squash merge carried its changes");
        // Once the board recorded a pull request, an unproven merge says so.
        plan.output
            .push(crate::output::Output { url: "https://github.com/group/app/pull/7".into(), ..Default::default() });
        assert_eq!(
            kept(&plan, "unmerged", "refs/heads/main").unwrap().reason,
            "no merge seen: its commits are not on main"
        );
        // The user's removal needs no proof for clean work, but uncommitted work always stays.
        let entry = |branch: &str| closed_tasks(&plan).unwrap().into_iter().find(|e| e.branch == branch).unwrap();
        assert_eq!(kept_work(&plan, &entry("unmerged"), Some("refs/heads/main"), true).unwrap(), None);
        assert!(!kept_work(&plan, &entry("dirty"), Some("refs/heads/main"), true).unwrap().unwrap().clean);
        // Without a default branch nothing is proven.
        assert!(kept_work(&plan, &entry("unmerged"), None, false).unwrap().is_some());
    }

    #[test]
    fn work_combined_into_a_squash_merged_branch_or_integrated_is_safe() {
        let (_dir, repo) = git::tests::project();
        let mut plan = plan(&repo, &["task"]);
        plan.status = PlanStatus::Completed;
        let task = worktree(&repo, "task");
        std::fs::write(task.join("a.txt"), "one\nTASK\nthree\n").unwrap();
        git::commit_all(&task, "task").unwrap();
        // The chief combines it into one branch whose review fix changes the same line again, and that
        // branch's pull request is squash-merged: the task's own change is no longer on main as it made it.
        git::git(&repo, &["branch", "combined", "task"]).unwrap();
        let combined = worktree_at(&repo, "combined");
        std::fs::write(combined.join("a.txt"), "one\nREVIEWED\nthree\n").unwrap();
        git::commit_all(&combined, "review fix").unwrap();
        squash(&repo, "combined");
        assert!(kept(&plan, "task", "refs/heads/main").is_some(), "no recorded branch contains it yet");
        plan.pull_request = Some(crate::plan::PullRequest {
            url: "https://github.com/group/app/pull/9".into(),
            number: 9,
            head: "combined".into(),
            base: "main".into(),
        });
        assert_eq!(kept(&plan, "task", "refs/heads/main"), None, "the merged pull request's branch contains it");
        // What the task did after it was combined is in no merged branch.
        commit(&task, "more-task-work");
        assert!(kept(&plan, "task", "refs/heads/main").is_some());
        // Integrated by ATOMICS: in the integration branch, which stays.
        let integration = plan.integration_branch();
        git::git(&repo, &["branch", &integration, "task"]).unwrap();
        assert_eq!(kept(&plan, "task", "refs/heads/main"), None);
    }

    #[test]
    fn the_integration_checkout_goes_only_once_merged_and_its_branch_always_stays() {
        let (_dir, repo) = git::tests::project();
        let mut plan = plan(&repo, &[]);
        let integration = plan.integration_branch();
        let checkout = worktree(&repo, &integration);
        commit(&checkout, "integrated-file");
        plan.integration_worktrees.push(crate::plan::IntegrationWorktree {
            repo: repo.clone(),
            branch: integration.clone(),
            path: checkout.clone(),
        });
        // A task on the integration branch, as when the chief made its final review work there.
        plan.tasks.push(Task {
            id: "review".into(),
            branch: Some(integration.clone()),
            worktree: Some(checkout.clone()),
            ..Task::default()
        });
        let entries = closed_tasks(&plan).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(kept(&plan, &integration, "refs/heads/main").is_some(), "not merged yet");
        git::git(&repo, &["merge", "-q", "--ff-only", &integration]).unwrap();
        assert_eq!(kept(&plan, &integration, "refs/heads/main"), None);
        let entry = closed_tasks(&plan).unwrap().remove(0);
        assert!(!remove_closed_task(&plan, &entry).unwrap(), "the integration branch is never deleted on close");
        assert!(!checkout.exists());
        assert!(git::branch_exists(&repo, &integration));
        // A worktree on the integration branch that the plan did not record is not its to remove.
        let other = worktree_at(&repo, &integration);
        plan.integration_worktrees.clear();
        plan.tasks.clear();
        assert!(closed_tasks(&plan).unwrap().is_empty());
        assert!(other.exists());
    }

    #[test]
    fn the_default_branch_is_fetched_before_it_is_checked() {
        let (dir, repo) = git::tests::project();
        let origin = dir.path().join("origin.git");
        let run = |cwd: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git").args(args).current_dir(cwd).status().unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(dir.path(), &["clone", "-q", "--bare", repo.to_str().unwrap(), origin.to_str().unwrap()]);
        run(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run(&repo, &["fetch", "-q", "origin"]);
        let mut plan = plan(&repo, &["task"]);
        plan.status = PlanStatus::Archived;
        let task = worktree(&repo, "task");
        commit(&task, "task-file");
        // Merged on the remote, which this machine has not fetched since.
        let before = git::git(&repo, &["rev-parse", "refs/remotes/origin/main"]).unwrap();
        run(&repo, &["push", "-q", "origin", "task:main"]);
        run(&repo, &["update-ref", "refs/remotes/origin/main", before.trim()]);
        assert!(kept(&plan, "task", "refs/remotes/origin/main").is_some(), "stale until fetched");
        let (default, warning) = default_branch(&repo, "origin/main");
        assert_eq!((default.as_deref(), warning), (Some("refs/remotes/origin/main"), None));
        assert_eq!(kept(&plan, "task", "refs/remotes/origin/main"), None);
    }

    /// A worktree on an existing `branch`, beside the repository.
    fn worktree_at(repo: &std::path::Path, branch: &str) -> PathBuf {
        let path = repo.with_file_name(format!("{}-checkout", branch.replace('/', "-")));
        git::git(repo, &["worktree", "add", "-q", path.to_str().unwrap(), branch]).unwrap();
        path
    }
}
