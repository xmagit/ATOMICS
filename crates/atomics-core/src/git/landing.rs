//! Integration admission and landing use host-recorded worktrees and immutable commits.
use super::*;

#[derive(Debug, Clone, Default)]
pub struct IntegrationOptions {
    pub commit: Option<String>,
    pub override_busy: bool,
    pub working: bool,
    /// Host receipt time of the last report, in nanoseconds since the epoch.
    pub reported_at: Option<u64>,
}

pub fn report_time() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
}

/// Refuse work that changed after the report, including deletions and restored mtimes.
/// Old plans without a precise report receipt time require another report or an override.
pub fn check_integration_ready(path: &Path, options: &IntegrationOptions) -> Result<()> {
    if options.override_busy {
        return Ok(());
    }
    ensure!(!options.working, "task session is working; wait for it to stop or pass override=true");
    if options.commit.is_some() || !path.exists() {
        return Ok(());
    }
    let status = git(path, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    if status.is_empty() {
        return Ok(());
    }
    let cutoff = options.reported_at.context(
        "task worktree is dirty and has no report for these changes; ask the agent to report or pass override=true",
    )?;
    let mut entries = status.split('\0').filter(|p| !p.is_empty());
    while let Some(entry) = entries.next() {
        ensure!(entry.len() >= 4, "could not read task worktree status");
        let mut paths = vec![&entry[3..]];
        if entry[..2].contains(['R', 'C']) {
            paths.push(entries.next().context("incomplete rename status")?);
        }
        for name in paths {
            let file = path.join(name);
            let metadata = match std::fs::symlink_metadata(&file) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // A deletion changes its containing directory, even with no remaining file.
                    let mut parent = file.parent().context("missing parent")?;
                    while !parent.exists() {
                        parent = parent.parent().context("missing worktree parent")?;
                    }
                    std::fs::symlink_metadata(parent)?
                }
                Err(error) => return Err(error.into()),
            };
            let modified = metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos();
            #[cfg(unix)]
            let changed = {
                use std::os::unix::fs::MetadataExt;
                (metadata.ctime() as u128) * 1_000_000_000 + metadata.ctime_nsec() as u128
            };
            #[cfg(not(unix))]
            let changed = modified;
            ensure!(
                modified.max(changed) <= cutoff as u128,
                "task worktree is dirty and {name} changed since the agent's last report; ask it to report or pass override=true"
            );
        }
    }
    Ok(())
}

/// Resolve a full object id and ensure it is a milestone of this task, not another branch.
pub fn task_commit(repo: &Path, branch: &str, requested: Option<&str>) -> Result<String> {
    let reference = format!("refs/heads/{branch}");
    let tip = commit_of(repo, &reference)?.context("task branch no longer exists")?;
    let Some(commit) = requested else { return Ok(tip) };
    ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|c| c.is_ascii_hexdigit()),
        "commit must be a full commit SHA"
    );
    let commit = commit_of(repo, &format!("{commit}^{{commit}}"))?.context("commit does not exist")?;
    ensure!(is_ancestor(repo, &commit, &tip)? == Some(true), "commit is not on task branch {branch}");
    Ok(commit)
}

/// Only advance the exact target we inspected before verification. A checked-out target is
/// updated with its worktree; Git's ff-only merge preserves dirty user work by refusing it.
pub fn land_fast_forward(repo: &Path, target: &str, expected: &str, verified: &str) -> Result<()> {
    let lock = worktree_lock(repo)?;
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    ensure!(git(repo, &["check-ref-format", "--branch", target]).is_ok(), "invalid landing target");
    let reference = format!("refs/heads/{target}");
    ensure!(
        git(repo, &["symbolic-ref", "--quiet", &reference]).is_err(),
        "landing target must be a direct local branch, not a symbolic ref"
    );
    ensure!(
        commit_of(repo, &reference)?.as_deref() == Some(expected),
        "landing target moved during verification; verify again"
    );
    ensure!(
        is_ancestor(repo, expected, verified)? == Some(true),
        "landing target cannot be fast-forwarded; merge it into the integration branch and verify again"
    );
    let trees: Vec<_> =
        list_worktrees(repo)?.into_iter().filter(|tree| tree.branch.as_deref() == Some(target)).collect();
    ensure!(trees.len() <= 1, "landing target has multiple worktree claims");
    if let Some(tree) = trees.first() {
        ensure!(tree.trusted, "landing target has an untrusted worktree");
        ensure!(git(&tree.path, &["status", "--porcelain"])?.is_empty(), "landing target worktree is dirty");
        git(&tree.path, &["merge", "--ff-only", "--no-edit", "--", verified])?;
    } else {
        git(repo, &["update-ref", "--no-deref", &reference, verified, expected])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::project;
    use super::*;

    fn integrate_task(repo: &Path, task: &Path, into: &Path, options: &IntegrationOptions) -> Result<Integration> {
        integrate_checked(
            repo,
            task,
            "task",
            "Task milestone",
            "integration",
            into,
            "main",
            &Default::default(),
            options,
        )
    }

    #[test]
    fn integration_refuses_working_and_dirty_unreported_without_committing() {
        let (dir, repo) = project();
        let (task, into) = (dir.path().join("task"), dir.path().join("integration"));
        add_worktree(&repo, &task, "task", "main").unwrap();
        let before = commit_of(&repo, "refs/heads/task").unwrap();
        let working = IntegrationOptions { working: true, ..Default::default() };
        assert!(integrate_task(&repo, &task, &into, &working).unwrap_err().to_string().contains("working"));
        std::fs::write(task.join("new.txt"), "unfinished").unwrap();
        assert!(
            integrate_task(&repo, &task, &into, &Default::default()).unwrap_err().to_string().contains("no report")
        );
        let reported = IntegrationOptions { reported_at: Some(report_time()), ..Default::default() };
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(task.join("new.txt"), "changed again").unwrap();
        assert!(integrate_task(&repo, &task, &into, &reported).unwrap_err().to_string().contains("since"));
        assert_eq!(commit_of(&repo, "refs/heads/task").unwrap(), before);
        assert!(!into.exists());
        assert!(git(&task, &["diff", "--cached", "--name-only"]).unwrap().is_empty());
        let reported = IntegrationOptions { reported_at: Some(report_time()), ..Default::default() };
        assert!(integrate_task(&repo, &task, &into, &reported).unwrap().committed);
        std::fs::write(task.join("new.txt"), "override knowingly").unwrap();
        let override_busy = IntegrationOptions { override_busy: true, working: true, ..Default::default() };
        assert!(integrate_task(&repo, &task, &into, &override_busy).unwrap().committed);
    }

    #[test]
    fn integration_refuses_deletions_and_renames_after_reporting() {
        let (dir, repo) = project();
        let task = dir.path().join("task");
        add_worktree(&repo, &task, "task", "main").unwrap();
        std::fs::write(task.join("old.txt"), "reported").unwrap();
        commit_all(&task, "old file").unwrap();
        let options = IntegrationOptions { reported_at: Some(report_time()), ..Default::default() };
        std::thread::sleep(Duration::from_millis(10));
        std::fs::rename(task.join("old.txt"), task.join("renamed.txt")).unwrap();
        git(&task, &["add", "-A"]).unwrap();
        assert!(check_integration_ready(&task, &options).is_err());
        std::fs::remove_file(task.join("renamed.txt")).unwrap();
        assert!(check_integration_ready(&task, &options).is_err());
    }

    #[test]
    fn integration_of_commit_leaves_later_commits_index_and_dirty_work_alone() {
        let (dir, repo) = project();
        let (task, into) = (dir.path().join("task"), dir.path().join("integration"));
        add_worktree(&repo, &task, "task", "main").unwrap();
        std::fs::write(task.join("milestone.txt"), "ready").unwrap();
        commit_all(&task, "milestone").unwrap();
        let milestone = commit_of(&task, "HEAD").unwrap().unwrap();
        std::fs::write(task.join("later.txt"), "later committed").unwrap();
        commit_all(&task, "later").unwrap();
        let head = commit_of(&task, "HEAD").unwrap();
        std::fs::write(task.join("unfinished.txt"), "half edited").unwrap();
        git(&task, &["add", "unfinished.txt"]).unwrap();
        std::fs::write(task.join("unfinished.txt"), "still editing").unwrap();
        let status = git(&task, &["status", "--porcelain"]).unwrap();
        let index = git(&task, &["write-tree"]).unwrap();
        let options = IntegrationOptions { commit: Some(milestone.clone()), ..Default::default() };
        let done = integrate_task(&repo, &task, &into, &options).unwrap();
        assert_eq!(done.commit, milestone);
        assert!(!done.committed);
        assert!(into.join("milestone.txt").exists());
        assert!(!into.join("later.txt").exists());
        assert!(!into.join("unfinished.txt").exists());
        assert_eq!(commit_of(&task, "HEAD").unwrap(), head);
        assert_eq!(git(&task, &["status", "--porcelain"]).unwrap(), status);
        assert_eq!(git(&task, &["write-tree"]).unwrap(), index);
        assert!(task_commit(&repo, "task", Some("HEAD")).is_err());
        git(&repo, &["commit", "--allow-empty", "-m", "unrelated"]).unwrap();
        assert!(task_commit(&repo, "task", commit_of(&repo, "HEAD").unwrap().as_deref()).is_err());
    }

    #[test]
    fn configured_plan_base_allows_dependents_and_later_integration() {
        let (dir, repo) = project();
        let task = dir.path().join("task");
        let into = dir.path().join("integration");
        add_worktree(&repo, &task, "task", "main").unwrap();
        std::fs::write(task.join("first.txt"), "first").unwrap();
        let options = IntegrationOptions { reported_at: Some(report_time()), ..Default::default() };
        integrate_task(&repo, &task, &into, &options).unwrap();
        let dependent = dir.path().join("dependent");
        add_worktree_from(&repo, &dependent, "dependent", Some("integration"), "integration").unwrap();
        assert!(dependent.join("first.txt").exists());
        std::fs::write(dependent.join("second.txt"), "second").unwrap();
        let options = IntegrationOptions { reported_at: Some(report_time()), ..Default::default() };
        let result = integrate_checked(
            &repo,
            &dependent,
            "dependent",
            "Second milestone",
            "integration",
            &into,
            "refs/heads/integration",
            &Default::default(),
            &options,
        )
        .unwrap();
        assert_eq!(result.merge, Merge::Merged);
        assert!(into.join("second.txt").exists());
        assert!(check_branch_name(&repo, "main", "integration").is_err());
        assert!(check_branch_name(&repo, "master", "integration").is_err());
        assert!(check_branch_name(&repo, "integration", "integration").is_err());
        assert!(check_integration_branch(&repo, "main", "main").is_err());
        assert!(check_integration_branch(&repo, "master", "master").is_err());
        assert!(check_integration_branch(&repo, "missing", "missing").is_err());
        assert!(check_integration_branch(&repo, "integration", "origin/integration").is_err());
        assert!(check_integration_branch(&repo, "integration", "refs/remotes/origin/integration").is_err());
    }

    #[test]
    fn landing_fast_forward_preserves_dirty_diverged_and_moved_targets() {
        let (dir, repo) = project();
        let before = commit_of(&repo, "HEAD").unwrap().unwrap();
        let task = dir.path().join("task");
        add_worktree(&repo, &task, "task", "main").unwrap();
        std::fs::write(task.join("feature.txt"), "ready").unwrap();
        commit_all(&task, "feature").unwrap();
        let verified = commit_of(&task, "HEAD").unwrap().unwrap();
        std::fs::write(repo.join("user.txt"), "keep").unwrap();
        assert!(land_fast_forward(&repo, "main", &before, &verified).unwrap_err().to_string().contains("dirty"));
        assert_eq!(commit_of(&repo, "HEAD").unwrap().as_deref(), Some(before.as_str()));
        std::fs::remove_file(repo.join("user.txt")).unwrap();
        git(&repo, &["branch", "release", &before]).unwrap();
        land_fast_forward(&repo, "release", &before, &verified).unwrap();
        assert_eq!(commit_of(&repo, "refs/heads/release").unwrap().as_deref(), Some(verified.as_str()));
        git(&repo, &["symbolic-ref", "refs/heads/alias", "refs/heads/release"]).unwrap();
        assert!(land_fast_forward(&repo, "alias", &verified, &verified).unwrap_err().to_string().contains("symbolic"));
        land_fast_forward(&repo, "main", &before, &verified).unwrap();
        assert!(repo.join("feature.txt").exists());
        assert!(land_fast_forward(&repo, "main", &before, &verified).unwrap_err().to_string().contains("moved"));
        assert!(land_fast_forward(&repo, "main", &verified, &before).unwrap_err().to_string().contains("fast-forward"));
    }
}
