//! Git through the `git` CLI: exact porcelain semantics, and it picks up
//! `core.fsmonitor`/`core.untrackedCache`, which matter on large repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};

use crate::diff::{FileDiff, WorktreeDiff, parse_unified};

const MAX_UNTRACKED_FILES: usize = 300;
const MAX_UNTRACKED_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub root: PathBuf,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The worktree containing `dir`, or `None` outside a git repository.
pub fn worktree_of(dir: &Path) -> Option<Worktree> {
    let root = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"]).ok()?.trim());
    // symbolic-ref also works before the first commit, unlike rev-parse --abbrev-ref HEAD.
    let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty());
    Some(Worktree { root, branch })
}

pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(WorktreeEntry { path: PathBuf::from(path), branch: None });
        } else if let Some(branch) = line.strip_prefix("branch ")
            && let Some(entry) = current.as_mut()
        {
            entry.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
        } else if line == "bare" {
            current = None;
        }
    }
    entries.extend(current);
    Ok(entries)
}

/// The merge-base with the first of `base_ref` and the usual fallbacks that exists.
pub fn merge_base(root: &Path, base_ref: &str) -> Result<(String, String)> {
    let candidates = [base_ref, "origin/main", "main", "origin/master", "master"];
    for candidate in candidates {
        if let Ok(out) = git(root, &["merge-base", candidate, "HEAD"]) {
            return Ok((candidate.to_string(), out.trim().to_string()));
        }
    }
    // A repository without commits (or without any main) shows everything as new.
    if git(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        return Ok(("(no commits)".to_string(), EMPTY_TREE.to_string()));
    }
    bail!("no merge-base with {base_ref} (or main/master) in {}", root.display())
}

/// Git's well-known id of the empty tree.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Everything this worktree changes relative to where it branched off `base_ref`:
/// commits, staged and unstaged edits, and untracked files. With `untracked_budget`,
/// untracked content beyond that many bytes is counted but left out.
pub fn diff_against_base(root: &Path, base_ref: &str, untracked_budget: Option<u64>) -> Result<WorktreeDiff> {
    let (resolved_ref, base) = merge_base(root, base_ref)?;
    let patch = git(
        root,
        &["diff", "--no-color", "--no-ext-diff", "--find-renames", "--src-prefix=a/", "--dst-prefix=b/", &base],
    )?;
    let mut files = parse_unified(&patch);
    let (untracked, partial) = untracked_files(root, untracked_budget)?;
    files.extend(untracked);
    files.sort_by(|a, b| a.path().cmp(b.path()));
    Ok(WorktreeDiff { base_ref: resolved_ref, base, files, partial })
}

fn untracked_files(root: &Path, budget: Option<u64>) -> Result<(Vec<FileDiff>, bool)> {
    let out = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut files = Vec::new();
    let (mut kept, mut partial) = (0, false);
    for path in out.split('\0').filter(|p| !p.is_empty()).take(MAX_UNTRACKED_FILES) {
        let full = root.join(path);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if !meta.is_file() {
            continue;
        }
        let content = if meta.len() <= MAX_UNTRACKED_BYTES { std::fs::read(&full).ok() } else { None };
        match content {
            Some(bytes) if !bytes[..bytes.len().min(8000)].contains(&0) => {
                let text = String::from_utf8_lossy(&bytes);
                if budget.is_some_and(|budget| kept + meta.len() > budget) {
                    files.push(FileDiff::untracked_omitted(path.to_string(), text.lines().count()));
                    partial = true;
                } else {
                    kept += meta.len();
                    files.push(FileDiff::untracked(path.to_string(), &text));
                }
            }
            _ => {
                let mut file = FileDiff::untracked(path.to_string(), "");
                file.binary = true;
                files.push(file);
            }
        }
    }
    Ok((files, partial))
}

/// The tracked files of a worktree. Untracked files only appear through the diff,
/// which keeps local clutter (logs, screenshots, scratch notes) out of the explorer.
pub fn list_files(root: &Path) -> Result<Vec<String>> {
    let out = git(root, &["ls-files", "-z", "--cached"])?;
    let mut files: Vec<String> = out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// The index file of the worktree at `root`; linked worktrees have their own. It is
/// rewritten whenever the tracked files change.
pub fn index_file(root: &Path) -> Result<PathBuf> {
    Ok(root.join(git(root, &["rev-parse", "--git-path", "index"])?.trim()))
}

/// What decides whether a worktree can go: uncommitted work, whether its branch is
/// merged or its remote branch was deleted (a completed PR), and its last commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeHealth {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Folder no longer exists; `git worktree prune` drops it.
    pub missing: bool,
    pub dirty_files: usize,
    pub merged: bool,
    pub upstream_gone: bool,
    pub last_commit: Option<String>,
}

impl WorktreeHealth {
    /// Clean, and its work is in main (merged, or the PR branch was deleted).
    pub fn is_safe_to_remove(&self) -> bool {
        !self.missing && self.dirty_files == 0 && (self.merged || self.upstream_gone)
    }
}

/// Health of every linked worktree of `repo` (the main checkout is left out).
pub fn worktree_health(repo: &Path, base_ref: &str) -> Result<Vec<WorktreeHealth>> {
    let (resolved, _) = merge_base(repo, base_ref)?;
    let merged: std::collections::HashSet<String> =
        git(repo, &["branch", "--format=%(refname:short)", "--merged", &resolved])?
            .lines()
            .map(str::to_string)
            .collect();
    let mut gone = std::collections::HashSet::new();
    let mut dates = std::collections::HashMap::new();
    for line in git(
        repo,
        &["for-each-ref", "--format=%(refname:short)%09%(upstream:track)%09%(committerdate:relative)", "refs/heads"],
    )?
    .lines()
    {
        let mut parts = line.split('\t');
        let (Some(name), Some(track), Some(date)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if track.contains("gone") {
            gone.insert(name.to_string());
        }
        dates.insert(name.to_string(), date.to_string());
    }
    let mut out = Vec::new();
    for entry in list_worktrees(repo)?.into_iter().skip(1) {
        let missing = !entry.path.is_dir();
        let dirty_files = if missing {
            0
        } else {
            git(&entry.path, &["status", "--porcelain", "--untracked-files=normal"])
                .map(|s| s.lines().count())
                .unwrap_or(0)
        };
        let branch = entry.branch.clone();
        let is = |set: &std::collections::HashSet<String>| branch.as_ref().is_some_and(|b| set.contains(b));
        out.push(WorktreeHealth {
            missing,
            dirty_files,
            merged: is(&merged),
            upstream_gone: is(&gone),
            last_commit: branch.as_ref().and_then(|b| dates.get(b).cloned()),
            branch,
            path: entry.path,
        });
    }
    Ok(out)
}

/// Removes a worktree without forcing (git refuses when it has uncommitted work),
/// and with `delete_branch` its branch, only when git considers it merged.
pub fn remove_worktree(repo: &Path, path: &Path, branch: Option<&str>, delete_branch: bool) -> Result<()> {
    git(repo, &["worktree", "remove", &path.to_string_lossy()])?;
    if delete_branch && let Some(branch) = branch {
        git(repo, &["branch", "-d", branch])?;
    }
    Ok(())
}

/// The first of `base_ref` and the usual fallbacks that exists, so repositories
/// without an `origin` (or without `main`) still work.
pub fn resolve_base(repo: &Path, base_ref: &str) -> Result<String> {
    for candidate in [base_ref, "origin/main", "main", "origin/master", "master"] {
        if git(repo, &["rev-parse", "--verify", "--quiet", &format!("{candidate}^{{commit}}")]).is_ok() {
            return Ok(candidate.to_string());
        }
    }
    bail!("none of {base_ref}, main or master exists in {}", repo.display())
}

/// Refuses branch names IMACS must never create or merge into: invalid ones, ones git
/// would read as options, and the base branch (main, master or `base_ref`'s branch).
pub fn check_branch_name(repo: &Path, branch: &str, base_ref: &str) -> Result<()> {
    if branch.starts_with('-') || git(repo, &["check-ref-format", "--branch", branch]).is_err() {
        bail!("{branch:?} is not a valid branch name");
    }
    let base = base_ref.strip_prefix("origin/").unwrap_or(base_ref);
    if ["main", "master", base].contains(&branch) {
        bail!("{branch} is the base branch; tasks and plans use branches of their own");
    }
    Ok(())
}

/// Creates `branch` from `base_ref` (or its fallback) in a new worktree at `path`.
pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base_ref: &str) -> Result<()> {
    let lock = worktree_lock(repo)?;
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    add_worktree_unlocked(repo, path, branch, base_ref)
}

fn worktree_lock(repo: &Path) -> Result<Arc<Mutex<()>>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);
    let common = git(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = std::fs::canonicalize(common.trim())?;
    let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    Ok(locks.entry(common).or_default().clone())
}

fn add_worktree_unlocked(repo: &Path, path: &Path, branch: &str, base_ref: &str) -> Result<()> {
    check_branch_name(repo, branch, base_ref)?;
    let base = resolve_base(repo, base_ref)?;
    let path_text = path.to_string_lossy();
    git(repo, &["worktree", "add", "--no-track", "-b", branch, &path_text, &base]).map(|_| ())
}

/// Like `add_worktree`, but from `start` when given, which is created from
/// `base_ref` first if it does not exist yet.
pub fn add_worktree_from(repo: &Path, path: &Path, branch: &str, start: Option<&str>, base_ref: &str) -> Result<()> {
    let lock = worktree_lock(repo)?;
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    match start {
        Some(start) => {
            ensure_branch(repo, start, base_ref)?;
            add_worktree_unlocked(repo, path, branch, start)
        }
        None => add_worktree_unlocked(repo, path, branch, base_ref),
    }
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

/// Creates `branch` from `base_ref` (or its fallback) unless it exists.
pub fn ensure_branch(repo: &Path, branch: &str, base_ref: &str) -> Result<()> {
    check_branch_name(repo, branch, base_ref)?;
    if branch_exists(repo, branch) {
        return Ok(());
    }
    let base = resolve_base(repo, base_ref)?;
    git(repo, &["branch", "--no-track", branch, &base]).map(|_| ())
}

/// Commits everything uncommitted in `worktree` (ignored files stay out) with the
/// repository's own identity. False when there was nothing to commit.
pub fn commit_all(worktree: &Path, message: &str) -> Result<bool> {
    if git(worktree, &["status", "--porcelain"])?.trim().is_empty() {
        return Ok(false);
    }
    git(worktree, &["add", "--all"])?;
    git(worktree, &["commit", "-q", "-m", message])?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    Merged,
    UpToDate,
    /// The merge was aborted; these files conflicted.
    Conflict(Vec<String>),
}

/// Merges `branch` into the branch checked out in `worktree`. A conflicting merge is
/// aborted, which leaves the worktree as it was.
pub fn merge(worktree: &Path, branch: &str) -> Result<Merge> {
    if !git(worktree, &["status", "--porcelain", "--untracked-files=no"])?.trim().is_empty() {
        bail!("{} has uncommitted changes", worktree.display());
    }
    let head = git(worktree, &["rev-parse", "HEAD"])?;
    if let Err(err) = git(worktree, &["merge", "--no-ff", "--no-edit", branch]) {
        let conflicts: Vec<String> = git(worktree, &["diff", "--name-only", "--diff-filter=U", "-z"])?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        if git(worktree, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_ok() {
            git(worktree, &["merge", "--abort"])?;
        }
        if conflicts.is_empty() {
            return Err(err);
        }
        return Ok(Merge::Conflict(conflicts));
    }
    Ok(if git(worktree, &["rev-parse", "HEAD"])? == head { Merge::UpToDate } else { Merge::Merged })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    /// Uncommitted work in the branch's worktree was committed first.
    pub committed: bool,
    pub merge: Merge,
    /// Where `into` is checked out.
    pub worktree: PathBuf,
}

/// Commits what is uncommitted in `branch`'s worktree and merges `branch` into `into`,
/// in a worktree of its own at `into_path`. `into` and its worktree are created (from
/// `base_ref`) when first needed. The main checkout is never touched.
pub fn integrate(
    repo: &Path,
    branch: &str,
    message: &str,
    into: &str,
    into_path: &Path,
    base_ref: &str,
) -> Result<Integration> {
    let lock = worktree_lock(repo)?;
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    check_branch_name(repo, branch, base_ref)?;
    check_branch_name(repo, into, base_ref)?;
    if !branch_exists(repo, branch) {
        bail!("branch {branch} does not exist in {}", repo.display());
    }
    let linked: Vec<WorktreeEntry> = list_worktrees(repo)?.into_iter().skip(1).collect();
    let checked_out = |name: &str| linked.iter().find(|e| e.branch.as_deref() == Some(name));
    let committed = match checked_out(branch).filter(|e| e.path.is_dir()) {
        Some(entry) => commit_all(&entry.path, message)?,
        None => false,
    };
    ensure_branch(repo, into, base_ref)?;
    let worktree = match checked_out(into) {
        Some(entry) if entry.path.is_dir() => entry.path.clone(),
        stale => {
            if stale.is_some() {
                // Its folder was deleted: drop the registration so the branch can be checked out again.
                git(repo, &["worktree", "prune"])?;
            }
            git(repo, &["worktree", "add", &into_path.to_string_lossy(), into])?;
            into_path.to_path_buf()
        }
    };
    let merge = merge(&worktree, branch)?;
    Ok(Integration { committed, merge, worktree })
}

/// Folder-name friendly version of a branch name.
pub fn slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Fills `{repo_parent}`, `{repo}` and `{slug}` in the configured worktree path.
pub fn worktree_path(template: &str, repo: &Path, branch: &str) -> PathBuf {
    let repo_name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = repo.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let filled =
        template.replace("{repo_parent}", &parent).replace("{repo}", &repo_name).replace("{slug}", &slug(branch));
    crate::paths::expand(&filled)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args).status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn init(p: &Path) {
        run(p, &["init", "-q", "-b", "main"]);
        run(p, &["config", "user.email", "IMACS@test"]);
        run(p, &["config", "user.name", "imacs"]);
        std::fs::write(p.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        run(p, &["add", "."]);
        run(p, &["commit", "-q", "-m", "base"]);
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path());
        dir
    }

    /// A repository at `<tmp>/app`, so worktrees next to it are cleaned up with it.
    pub(crate) fn project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir(&repo).unwrap();
        init(&repo);
        (dir, repo)
    }

    #[test]
    fn new_worktrees_never_track_the_remote_base() {
        let (_dir, repo) = project();
        git(&repo, &["remote", "add", "origin", "."]).unwrap();
        git(&repo, &["fetch", "origin", "main"]).unwrap();
        git(&repo, &["config", "branch.autoSetupMerge", "always"]).unwrap();
        for (branch, start) in [("direct", None), ("from-base", None), ("dependent", Some("integration"))] {
            let path = repo.with_file_name(branch);
            if branch == "direct" {
                add_worktree(&repo, &path, branch, "origin/main").unwrap();
            } else {
                add_worktree_from(&repo, &path, branch, start, "origin/main").unwrap();
            }
            assert!(git(&path, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_err(), "{branch}");
        }
    }

    #[test]
    fn worktree_creation_is_serialized_across_checkouts_of_one_repository() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc::channel;
        use std::time::{Duration, Instant};

        let (_dir, repo) = project();
        let linked = repo.with_file_name("linked");
        add_worktree(&repo, &linked, "linked", "main").unwrap();
        let lock = worktree_lock(&repo).unwrap();
        assert!(Arc::ptr_eq(&lock, &worktree_lock(&linked).unwrap()));
        let started = repo.join(".git/creation-started");
        let release = repo.join(".git/creation-release");
        let hook = repo.join(".git/hooks/reference-transaction");
        std::fs::write(&hook, format!(
            "#!/bin/sh\n[ \"$1\" = prepared ] || exit 0\n: > '{}'\nattempts=0\nwhile [ ! -f '{}' ]; do\n  attempts=$((attempts + 1))\n  [ \"$attempts\" -lt 1000 ] || exit 1\n  sleep 0.01\ndone\n",
            started.display(), release.display(),
        )).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, rx) = channel();
        std::thread::scope(|scope| {
            let tx_first = tx.clone();
            let repo_first = &repo;
            scope.spawn(move || {
                tx_first
                    .send(add_worktree_from(
                        &linked,
                        &repo_first.with_file_name("task-0"),
                        "task-0",
                        Some("integration"),
                        "main",
                    ))
                    .unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while !started.exists() {
                assert!(Instant::now() < deadline, "worktree creation did not reach the reference hook");
                std::thread::sleep(Duration::from_millis(10));
            }
            // Creation itself holds the common lock while git is paused in the hook.
            assert!(matches!(lock.try_lock(), Err(std::sync::TryLockError::WouldBlock)));
            let repo_second = &repo;
            scope.spawn(move || {
                tx.send(add_worktree_from(
                    repo_second,
                    &repo_second.with_file_name("task-1"),
                    "task-1",
                    Some("integration"),
                    "main",
                ))
                .unwrap();
            });
            let (_other_dir, other) = project();
            assert!(!Arc::ptr_eq(&lock, &worktree_lock(&other).unwrap()));
            add_worktree(&other, &other.with_file_name("parallel"), "parallel", "main").unwrap();
            std::fs::write(&release, "").unwrap();
            for _ in 0..2 {
                rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            }
        });
    }

    #[test]
    fn integrate_commits_the_task_and_merges_it_in_a_worktree_of_its_own() {
        let (_dir, repo) = project();
        let task = repo.with_file_name("app-t1");
        add_worktree(&repo, &task, "imacs/t1-feature", "main").unwrap();
        std::fs::write(task.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(task.join("b.txt"), "feature\n").unwrap();
        std::fs::write(task.join("debug.log"), "noise\n").unwrap();
        let main = git(&repo, &["rev-parse", "main"]).unwrap();

        let into = worktree_path("{repo_parent}/{repo}-{slug}", &repo, "imacs/plan-ship");
        let done = integrate(&repo, "imacs/t1-feature", "Add b (t1)", "imacs/plan-ship", &into, "origin/main").unwrap();
        assert_eq!(done, Integration { committed: true, merge: Merge::Merged, worktree: into.clone() });
        assert_eq!(git(&task, &["log", "-1", "--format=%s by %an"]).unwrap().trim(), "Add b (t1) by imacs");
        assert!(git(&task, &["status", "--porcelain"]).unwrap().is_empty());
        assert_eq!(worktree_of(&into).unwrap().branch.as_deref(), Some("imacs/plan-ship"));
        assert!(into.join("b.txt").exists());
        assert!(!into.join("debug.log").exists(), "ignored files are never committed");

        // The main checkout stays where it was.
        assert_eq!(worktree_of(&repo).unwrap().branch.as_deref(), Some("main"));
        assert_eq!(git(&repo, &["rev-parse", "main"]).unwrap(), main);
        assert!(!repo.join("b.txt").exists());

        let again = integrate(&repo, "imacs/t1-feature", "Add b (t1)", "imacs/plan-ship", &into, "main").unwrap();
        assert_eq!((again.committed, again.merge), (false, Merge::UpToDate));
    }

    #[test]
    fn conflicting_integration_is_aborted_and_lists_the_files() {
        let (_dir, repo) = project();
        for (branch, text) in [("one", "one\nTWO\nthree\n"), ("two", "one\n2\nthree\n")] {
            let worktree = repo.with_file_name(format!("app-{branch}"));
            add_worktree(&repo, &worktree, branch, "main").unwrap();
            std::fs::write(worktree.join("a.txt"), text).unwrap();
        }
        let into = repo.with_file_name("app-plan");
        assert_eq!(integrate(&repo, "one", "one (t1)", "plan", &into, "main").unwrap().merge, Merge::Merged);
        let head = git(&into, &["rev-parse", "HEAD"]).unwrap();

        let clash = integrate(&repo, "two", "two (t2)", "plan", &into, "main").unwrap();
        assert_eq!(clash.merge, Merge::Conflict(vec!["a.txt".into()]));
        assert!(clash.committed, "the task's work is kept on its branch");
        assert_eq!(git(&into, &["rev-parse", "HEAD"]).unwrap(), head);
        assert!(git(&into, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(git(&into, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_err());
        assert_eq!(std::fs::read_to_string(into.join("a.txt")).unwrap(), "one\nTWO\nthree\n");
    }

    #[test]
    fn diff_covers_commits_edits_and_untracked_but_not_main_moving() {
        let dir = repo();
        let p = dir.path();
        run(p, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(p.join("b.txt"), "committed\n").unwrap();
        run(p, &["add", "."]);
        run(p, &["commit", "-q", "-m", "feature work"]);
        std::fs::write(p.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(p.join("c.txt"), "untracked\n").unwrap();

        // main moves on after the branch point; that must not show up.
        run(p, &["checkout", "-q", "main"]);
        std::fs::write(p.join("main-only.txt"), "main\n").unwrap();
        run(p, &["add", "main-only.txt"]);
        run(p, &["commit", "-q", "-m", "main moves"]);
        run(p, &["checkout", "-q", "feature"]);

        let diff = diff_against_base(p, "main", None).unwrap();
        let paths: Vec<&str> = diff.files.iter().map(|f| f.path()).collect();
        assert_eq!(paths, ["a.txt", "b.txt", "c.txt"]);
        assert_eq!(diff.base_ref, "main");
        assert_eq!(diff.files[2].status, crate::diff::FileStatus::Untracked);
        assert_eq!((diff.additions(), diff.deletions()), (3, 1));
        assert!(!diff.partial);
    }

    #[test]
    fn untracked_content_beyond_the_budget_is_counted_but_left_out() {
        let dir = repo();
        let p = dir.path();
        std::fs::write(p.join("b.txt"), "1\n2\n").unwrap();
        std::fs::write(p.join("c.txt"), "1\n2\n3\n").unwrap();

        let diff = diff_against_base(p, "main", Some(5)).unwrap();
        assert!(diff.partial);
        let (b, c) = (&diff.files[0], &diff.files[1]);
        assert_eq!((b.hunks.len(), b.omitted), (1, 0));
        assert_eq!((c.hunks.len(), c.omitted), (0, 3));
        assert_eq!(diff.additions(), 5);
        assert!(!diff_against_base(p, "main", None).unwrap().partial);
    }

    #[test]
    fn index_file_is_per_worktree() {
        let (_dir, repo) = project();
        let worktree = repo.with_file_name("app-feature");
        add_worktree(&repo, &worktree, "feature", "main").unwrap();
        let (main, linked) = (index_file(&repo).unwrap(), index_file(&worktree).unwrap());
        assert!(main.is_file() && linked.is_file());
        assert_ne!(main, linked);
    }

    #[test]
    fn falls_back_when_base_ref_is_missing() {
        let dir = repo();
        let diff = diff_against_base(dir.path(), "origin/does-not-exist", None).unwrap();
        assert_eq!(diff.base_ref, "main");
        assert!(diff.files.is_empty());
    }

    #[test]
    fn worktrees_are_listed_with_branches() {
        let dir = repo();
        let p = dir.path();
        let wt = p.parent().unwrap().join(format!("{}-feat", p.file_name().unwrap().to_string_lossy()));
        add_worktree(p, &wt, "feat/x", "main").unwrap();
        let entries = list_worktrees(p).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].branch.as_deref(), Some("feat/x"));
        let info = worktree_of(&wt).unwrap();
        assert_eq!(info.branch.as_deref(), Some("feat/x"));
        std::fs::remove_dir_all(&wt).ok();
    }

    #[test]
    fn repository_without_commits_diffs_against_the_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        assert_eq!(worktree_of(dir.path()).unwrap().branch.as_deref(), Some("main"));
        std::fs::write(dir.path().join("new.rs"), "fn main() {}\n").unwrap();
        let diff = diff_against_base(dir.path(), "origin/main", None).unwrap();
        assert_eq!(diff.base_ref, "(no commits)");
        assert_eq!(diff.files.len(), 1);
    }

    #[test]
    fn health_marks_merged_clean_worktrees_safe_and_dirty_ones_not() {
        let dir = repo();
        let p = dir.path();
        let base = p.parent().unwrap().join(format!("{}-wt", p.file_name().unwrap().to_string_lossy()));
        let (merged, dirty) = (base.with_extension("merged"), base.with_extension("dirty"));
        add_worktree(p, &merged, "done", "main").unwrap();
        add_worktree(p, &dirty, "busy", "main").unwrap();
        std::fs::write(dirty.join("wip.txt"), "x").unwrap();

        let health = worktree_health(p, "main").unwrap();
        let find = |name: &str| health.iter().find(|h| h.branch.as_deref() == Some(name)).unwrap().clone();
        assert!(find("done").is_safe_to_remove());
        assert_eq!(find("busy").dirty_files, 1);
        assert!(!find("busy").is_safe_to_remove());

        remove_worktree(p, &merged, Some("done"), true).unwrap();
        assert!(!merged.exists());
        assert!(git(p, &["rev-parse", "--verify", "--quiet", "refs/heads/done"]).is_err());
        assert!(remove_worktree(p, &dirty, Some("busy"), true).is_err(), "dirty worktrees are kept");
        std::fs::remove_dir_all(&dirty).ok();
    }

    #[test]
    fn worktrees_branch_from_main_when_there_is_no_origin() {
        let dir = repo();
        let p = dir.path();
        let wt = p.parent().unwrap().join(format!("{}-no-origin", p.file_name().unwrap().to_string_lossy()));
        add_worktree(p, &wt, "feature/x", "origin/main").unwrap();
        assert_eq!(worktree_of(&wt).unwrap().branch.as_deref(), Some("feature/x"));
        std::fs::remove_dir_all(&wt).ok();
    }

    #[test]
    fn base_branches_and_option_like_names_are_refused() {
        let dir = repo();
        let p = dir.path();
        for bad in ["main", "master", "-D", "has space", "a..b", "release"] {
            let base_ref = if bad == "release" { "origin/release" } else { "origin/main" };
            assert!(check_branch_name(p, bad, base_ref).is_err(), "{bad} must be refused");
        }
        assert!(check_branch_name(p, "imacs/plan-ship", "origin/main").is_ok());
        let wt = p.parent().unwrap().join(format!("{}-bad", p.file_name().unwrap().to_string_lossy()));
        assert!(add_worktree(p, &wt, "-D", "main").is_err());
        assert!(!wt.exists());
    }

    #[test]
    fn worktree_path_template() {
        let path = worktree_path("{repo_parent}/{repo}-{slug}", Path::new("/r/shop"), "issue/42-fix it");
        assert_eq!(path, PathBuf::from("/r/shop-issue-42-fix-it"));
    }
}
