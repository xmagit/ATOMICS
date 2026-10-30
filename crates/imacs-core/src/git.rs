//! Git through the `git` CLI: exact porcelain semantics, and it picks up
//! `core.fsmonitor`/`core.untrackedCache`, which matter on large repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

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
    let output = command(dir, args)?.output().with_context(|| format!("running git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git -C dir args`. In a folder a sandbox works in, git runs with the locations [`pin`]ned
/// for it, never the ones the folder names now.
pub fn command(dir: &Path, args: &[&str]) -> Result<Command> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(["-c", "core.quotepath=off"]);
    match pinned(dir) {
        None => {
            command.args(args);
        }
        Some(GitPins { work_tree, git_dir: Some(git_dir), common_dir: Some(common_dir) }) => {
            command.env("GIT_DIR", git_dir).env("GIT_COMMON_DIR", common_dir).env("GIT_WORK_TREE", work_tree);
            // A submodule the sandbox adds would run git in a folder it chose, with that folder's
            // config. Only the command-line flag overrides the `ignore` its .gitmodules can set.
            match args.split_first() {
                Some((&subcommand, rest)) if subcommand == "status" || subcommand == "diff" => {
                    command.arg(subcommand).arg("--ignore-submodules=dirty").args(rest);
                }
                _ => {
                    command.args(args);
                }
            }
        }
        Some(pins) => bail!(
            "{} was outside any git repository when its sandbox started, so git does not run there",
            pins.work_tree.display()
        ),
    }
    Ok(command)
}

/// Where git finds the repository of a folder a sandbox works in, recorded when its session
/// starts. The sandbox can write the files git otherwise reads for this: a worktree's `.git`
/// file, its `commondir`, a main checkout's `.git/commondir`. Followed on this machine, they
/// lead to a repository config of the sandbox's own, whose `core.fsmonitor` git runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPins {
    pub work_tree: PathBuf,
    /// `None` when the folder was outside any repository: git never runs there.
    pub git_dir: Option<PathBuf>,
    pub common_dir: Option<PathBuf>,
}

static PINNED: LazyLock<Mutex<Vec<GitPins>>> = LazyLock::new(Mutex::default);

/// From now on, git in this process runs in `pins.work_tree` with these locations.
pub fn pin(pins: GitPins) {
    let mut pinned = PINNED.lock().unwrap_or_else(|e| e.into_inner());
    pinned.retain(|other| other.work_tree != pins.work_tree);
    pinned.push(pins);
}

pub fn unpin(work_tree: &Path) {
    PINNED.lock().unwrap_or_else(|e| e.into_inner()).retain(|pins| pins.work_tree != work_tree);
}

/// The pins of the innermost pinned folder holding `dir`, by its path as given or as it resolves.
pub fn pinned(dir: &Path) -> Option<GitPins> {
    let pinned = PINNED.lock().unwrap_or_else(|e| e.into_inner());
    if pinned.is_empty() {
        return None;
    }
    let real = std::fs::canonicalize(dir).ok();
    pinned
        .iter()
        .filter(|pins| {
            dir.starts_with(&pins.work_tree) || real.as_ref().is_some_and(|r| r.starts_with(&pins.work_tree))
        })
        .max_by_key(|pins| pins.work_tree.components().count())
        .cloned()
}

/// Where git finds `dir`'s repository now, read from the folder itself, never from a record:
/// for [`pin`] before a sandbox can write there, or to check a record against. Refuses layouts
/// that already point where `git worktree add` never would, and repositories with
/// `extensions.worktreeConfig`. rev-parse runs no hooks or fsmonitor, whatever the folder says.
pub fn resolve_pins(dir: &Path) -> Result<GitPins> {
    let args = ["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-dir", "--git-common-dir"];
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git rev-parse")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("not a git repository") {
            return Ok(GitPins { work_tree: std::fs::canonicalize(dir)?, git_dir: None, common_dir: None });
        }
        bail!("git rev-parse failed in {}: {}", dir.display(), error.trim());
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = output.lines().collect();
    let [top, git_dir, common] = lines[..] else { bail!("unexpected git rev-parse output: {output}") };
    let real = |path: &str| std::fs::canonicalize(path).with_context(|| format!("resolving {path}"));
    let (top, git_dir, common) = (real(top)?, real(git_dir)?, real(common)?);
    let made_by_git = if git_dir == common {
        (git_dir == top.join(".git") || !git_dir.starts_with(&top)) && !in_writable_git_folder(&git_dir)
    } else {
        // `git worktree add` makes <common>/worktrees/<id>, pointing back at the worktree's .git file.
        let back = std::fs::read_to_string(git_dir.join("gitdir")).unwrap_or_default();
        git_dir.parent() == Some(common.join("worktrees").as_path())
            && !common.starts_with(&top)
            && !in_writable_git_folder(&common)
            && std::fs::canonicalize(git_dir.join(back.trim())).ok() == Some(top.join(".git"))
    };
    ensure!(made_by_git, "{}'s git folder {} is not one git made for it", top.display(), git_dir.display());
    check_worktree_config(&common)?;
    Ok(GitPins { work_tree: top, git_dir: Some(git_dir), common_dir: Some(common) })
}

/// Whether `dir` lies in the objects, refs, logs or worktrees of another git folder: sandboxes
/// write there, and git never puts a repository there.
fn in_writable_git_folder(dir: &Path) -> bool {
    dir.ancestors().skip(1).any(|ancestor| {
        ancestor
            .file_name()
            .is_some_and(|name| ["objects", "refs", "logs", "worktrees"].contains(&&*name.to_string_lossy()))
            && ancestor.parent().is_some_and(|parent| parent.join("HEAD").is_file())
    })
}

/// With `extensions.worktreeConfig`, git reads `config.worktree` in a worktree's own git folder,
/// which its sandbox can write, as repository config on this machine.
pub fn check_worktree_config(common: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["config", "--type=bool", "--file"])
        .arg(common.join("config"))
        .args(["--get", "extensions.worktreeConfig"])
        .current_dir(common)
        .output()
        .context("running git config")?;
    if String::from_utf8_lossy(&output.stdout).trim() == "true" {
        bail!(
            "{} sets extensions.worktreeConfig, which lets a sandbox change git config that runs on this \
             machine. Unset it, or set isolation = \"bare\" for agents in this repository.",
            common.display()
        );
    }
    Ok(())
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
        let (len, content) = match read_worktree_file(root, path, MAX_UNTRACKED_BYTES) {
            Some(WorktreeFile::Link(target)) => (target.len() as u64, Some(target.into_bytes())),
            Some(WorktreeFile::File { len, bytes }) => (len, bytes),
            None => continue,
        };
        match content {
            Some(bytes) if !bytes[..bytes.len().min(8000)].contains(&0) => {
                let text = String::from_utf8_lossy(&bytes);
                if budget.is_some_and(|budget| kept + len > budget) {
                    files.push(FileDiff::untracked_omitted(path.to_string(), text.lines().count()));
                    partial = true;
                } else {
                    kept += len;
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

/// A file in a worktree, as git sees it.
#[derive(Debug, PartialEq, Eq)]
pub enum WorktreeFile {
    /// A symbolic link, shown as its target path the way git diffs one.
    Link(String),
    /// A regular file of `len` bytes; `None` when it is longer than the limit it was read with.
    File { len: u64, bytes: Option<Vec<u8>> },
}

/// Reads `path` in the worktree at `root` without following a symbolic link anywhere below
/// `root`, as git does: a sandbox can point one at any file on this machine. `None` for what
/// git would not show as a file there: a folder, a special file, or a path through a link.
pub fn read_worktree_file(root: &Path, path: &str, limit: u64) -> Option<WorktreeFile> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut full = root.to_path_buf();
    let mut parts = Path::new(path).components().peekable();
    while let Some(part) = parts.next() {
        let std::path::Component::Normal(name) = part else { return None };
        full.push(name);
        let kind = full.symlink_metadata().ok()?.file_type();
        match (kind.is_symlink(), parts.peek().is_some()) {
            (true, false) => return Some(WorktreeFile::Link(std::fs::read_link(&full).ok()?.to_string_lossy().into())),
            (true, true) => return None,
            (false, true) if !kind.is_dir() => return None,
            (false, false) if !kind.is_file() => return None,
            _ => {}
        }
    }
    // The checks above can race with the sandbox: refuse a link here, and never wait on a FIFO.
    let file =
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&full).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    let bytes = (meta.len() <= limit && file.take(limit).read_to_end(&mut bytes).is_ok()).then_some(bytes);
    Some(WorktreeFile::File { len: meta.len(), bytes })
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
    let path_text = path.to_string_lossy();
    if let Some(pins) = pinned(path).filter(|_| path.exists()) {
        // `worktree remove` checks for changes with a git of its own, which follows the worktree's
        // .git file and commondir. Check with the pinned locations, as strictly, then force.
        ensure!(
            git(path, &["status", "--porcelain"])?.trim().is_empty(),
            "'{path_text}' contains modified or untracked files"
        );
        let submodule =
            git(path, &["ls-files", "--stage", "-z"])?.split('\0').any(|entry| entry.starts_with("160000 "));
        ensure!(!submodule, "'{path_text}' contains submodules, which worktree removal leaves to you");
        let git_dir = pins.git_dir.context("worktree has no pinned git directory")?;
        let common = pins.common_dir.context("worktree has no pinned common directory")?;
        ensure!(git_dir != common, "the main checkout cannot be removed");
        ensure!(!git_dir.join("locked").exists(), "'{path_text}' is locked");
        // Git validates both directions even with --force. Replace files by rename so a
        // sandbox-created symlink cannot redirect these writes to a host file. The
        // recorded locations are trusted; neither .git nor commondir is rediscovered.
        replace_git_file(&pins.work_tree.join(".git"), &format!("gitdir: {}\n", git_dir.display()))?;
        replace_git_file(&git_dir.join("commondir"), &format!("{}\n", common.display()))?;
        replace_git_file(&git_dir.join("gitdir"), &format!("{}\n", pins.work_tree.join(".git").display()))?;
        git(repo, &["worktree", "remove", "--force", &path_text])?;
    } else {
        git(repo, &["worktree", "remove", &path_text])?;
    }
    if delete_branch && let Some(branch) = branch {
        git(repo, &["branch", "-d", branch])?;
    }
    Ok(())
}

fn replace_git_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().context("git file has no parent")?)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.persist(path).map_err(|error| error.error)?;
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

/// Canonical repository identity shared by its main checkout and linked worktrees.
pub fn common_dir(repo: &Path) -> Result<PathBuf> {
    let common = git(repo, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(std::fs::canonicalize(common.trim())?)
}

fn worktree_lock(repo: &Path) -> Result<Arc<Mutex<()>>> {
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(Mutex::default);
    let common = common_dir(repo)?;
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

/// Best-effort cache warming, called on the worktree job before publishing it for launch.
/// The first worktree is Git's main checkout, even when `repo` is a linked worktree.
/// Existing destinations (including retries) are never overwritten.
pub fn warm_worktree(repo: &Path, worktree: &Path, folders: &[String]) -> bool {
    if folders.is_empty() {
        return false;
    }
    let Some(main) = list_worktrees(repo).ok().and_then(|trees| trees.into_iter().next()) else { return false };
    let Ok(main) = main.path.canonicalize() else { return false };
    let mut warmed = false;
    for folder in folders {
        let relative = Path::new(folder);
        // Refuse absolute paths, traversal and Git metadata. Nested cache folders are fine.
        if relative.as_os_str().is_empty()
            || !relative.components().all(|part| matches!(part, std::path::Component::Normal(name) if name != ".git"))
        {
            continue;
        }
        let source = main.join(relative);
        let destination = worktree.join(relative);
        if !source.symlink_metadata().is_ok_and(|meta| meta.is_dir())
            || !source.canonicalize().is_ok_and(|path| path.starts_with(&main))
            || destination.symlink_metadata().is_ok()
        {
            continue;
        }
        // Do not follow destination symlinks out of the new checkout.
        let mut parent = worktree.to_path_buf();
        let safe = relative.parent().unwrap().components().all(|part| {
            parent.push(part);
            !parent.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink())
        });
        if safe && std::fs::create_dir_all(&parent).is_ok() {
            warmed |= clone_directory(&source, &destination);
        }
    }
    warmed
}

/// GNU cp must fail rather than fall back to copying file data.
#[cfg(any(target_os = "linux", test))]
fn linux_clone_command(source: &Path, destination: &Path) -> Command {
    let mut command = Command::new("cp");
    command.args(["-R", "--reflink=always"]).arg(source).arg(destination);
    command
}

/// Clone regular files with the native clone-only API. Walk directories ourselves:
/// Darwin discourages passing directory hierarchies directly to clonefile(2).
/// Symlinks are recreated, never followed into other folders or filesystems.
#[cfg(target_os = "macos")]
fn clone_cache_contents(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let metadata = source.symlink_metadata()?;
    if metadata.is_dir() {
        std::fs::create_dir(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            clone_cache_contents(&entry.path(), &destination.join(entry.file_name()))?;
        }
        std::fs::set_permissions(destination, metadata.permissions())
    } else if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(source)?, destination)
    } else if metadata.is_file() {
        let source = CString::new(source.as_os_str().as_bytes())?;
        let destination = CString::new(destination.as_os_str().as_bytes())?;
        // SAFETY: both paths are valid, NUL-terminated strings, live for this call.
        // clonefile has no full-copy fallback; errors fail this cache folder open.
        if unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "cache contains a special file"))
    }
}

#[cfg(target_os = "linux")]
fn clone_cache_contents(source: &Path, destination: &Path) -> std::io::Result<()> {
    // GNU cp recreates sockets/FIFOs even with --reflink=always. Match the
    // native macOS walker: cache warming must skip those trees entirely.
    check_cache_types(source)?;
    let status = linux_clone_command(source, destination)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other("CoW clone failed")) }
}

#[cfg(target_os = "linux")]
fn check_cache_types(path: &Path) -> std::io::Result<()> {
    let kind = path.symlink_metadata()?.file_type();
    if kind.is_file() || kind.is_symlink() {
        return Ok(());
    }
    if !kind.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "cache contains a special file"));
    }
    for entry in std::fs::read_dir(path)? {
        check_cache_types(&entry?.path())?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_cache_contents(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no CoW clone support"))
}

fn clone_directory(source: &Path, destination: &Path) -> bool {
    clone_directory_with(source, destination, clone_cache_contents)
}

fn clone_directory_with(
    source: &Path,
    destination: &Path,
    clone: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> bool {
    if destination.symlink_metadata().is_ok() {
        return false;
    }
    // A failed clone can leave a partial tree. Stage beside the destination so only a
    // complete clone is published, and TempDir removes partial output without a retry.
    let Some(parent) = destination.parent() else { return false };
    let Ok(stage) = tempfile::Builder::new().prefix(".imacs-warm-").tempdir_in(parent) else { return false };
    let cloned = stage.path().join("cache");
    clone(source, &cloned).is_ok() && std::fs::rename(cloned, destination).is_ok()
}

/// Exercise the same clone operation on the repository/worktree filesystems, using
/// temporary directories only. Doctor calls this explicitly; launch has no extra probe.
pub fn probe_cache_clone(source_folder: &Path, destination_folder: &Path) -> Result<bool> {
    let source = tempfile::Builder::new().prefix(".imacs-cow-").tempdir_in(source_folder)?;
    std::fs::write(source.path().join("probe"), "copy-on-write probe")?;
    let destination = tempfile::Builder::new().prefix(".imacs-cow-").tempdir_in(destination_folder)?;
    Ok(clone_directory(source.path(), &destination.path().join("cache")))
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

    fn repo() -> crate::test_support::TempDir {
        let dir = crate::test_support::tempdir().unwrap();
        init(dir.path());
        dir
    }

    /// A repository at `<tmp>/app`, so worktrees next to it are cleaned up with it.
    pub(crate) fn project() -> (crate::test_support::TempDir, PathBuf) {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir(&repo).unwrap();
        init(&repo);
        (dir, repo)
    }

    /// What a sandbox can plant in a git folder it writes: a git folder of its own that sees the
    /// repository's objects and refs, with a config whose fsmonitor and hooks leave `marker`.
    fn plant_hostile_git_dir(evil: &Path, real: &Path, head: &str, marker: &Path) {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::create_dir_all(evil.join("hooks")).unwrap();
        symlink(real.join("objects"), evil.join("objects")).unwrap();
        symlink(real.join("refs"), evil.join("refs")).unwrap();
        std::fs::write(evil.join("HEAD"), format!("ref: {head}\n")).unwrap();
        let hook = evil.join("hooks/reference-transaction");
        std::fs::write(&hook, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = format!(
            "[core]\n\trepositoryformatversion = 0\n\tfsmonitor = {}\n\thooksPath = {}\n",
            hook.display(),
            evil.join("hooks").display()
        );
        std::fs::write(evil.join("config"), config).unwrap();
    }

    #[derive(Debug, Clone, Copy)]
    enum Redirect {
        /// A linked worktree's `.git` file names a git folder of the sandbox's own.
        GitFile,
        /// A linked worktree's `<common>/worktrees/<id>/commondir` names another common dir.
        WorktreeCommondir,
        /// A main checkout's `.git/commondir` does.
        MainCommondir,
    }

    #[test]
    fn pinned_git_never_follows_a_sandboxs_redirects_to_its_own_config() {
        for redirect in [Redirect::GitFile, Redirect::WorktreeCommondir, Redirect::MainCommondir] {
            let (dir, repo) = project();
            let marker = dir.path().join("hook-ran");
            let folder = match redirect {
                Redirect::MainCommondir => repo.clone(),
                _ => {
                    let worktree = repo.with_file_name("app-task");
                    add_worktree(&repo, &worktree, "task", "main").unwrap();
                    worktree
                }
            };
            // Recorded when the session starts, before the sandbox has written anything.
            let pins = resolve_pins(&folder).unwrap();
            let (git_dir, common) = (pins.git_dir.clone().unwrap(), pins.common_dir.clone().unwrap());
            assert_eq!(pins.work_tree, folder.canonicalize().unwrap());
            assert_eq!(common, repo.join(".git").canonicalize().unwrap());

            let evil = git_dir.join("evil");
            let head = if matches!(redirect, Redirect::MainCommondir) { "refs/heads/main" } else { "refs/heads/task" };
            plant_hostile_git_dir(&evil, &common, head, &marker);
            match redirect {
                Redirect::GitFile => std::fs::write(folder.join(".git"), format!("gitdir: {}\n", evil.display())),
                Redirect::WorktreeCommondir | Redirect::MainCommondir => {
                    std::fs::write(git_dir.join("commondir"), format!("{}\n", evil.display()))
                }
            }
            .unwrap();
            std::fs::write(folder.join("a.txt"), "one\nTWO\nthree\n").unwrap();

            // Unpinned, the diff IMACS polls would run the sandbox's fsmonitor, and a tampered
            // layout no longer passes as one git made.
            assert!(resolve_pins(&folder).is_err(), "{redirect:?}");
            diff_against_base(&folder, "main", None).unwrap();
            assert!(marker.exists(), "{redirect:?}: the planted config must be live for this test to mean anything");
            std::fs::remove_file(&marker).unwrap();

            pin(pins.clone());
            let diff = diff_against_base(&folder, "main", None).unwrap();
            assert_eq!(diff.files.iter().map(FileDiff::path).collect::<Vec<_>>(), ["a.txt"], "{redirect:?}");
            assert_eq!(worktree_of(&folder).unwrap().branch.as_deref(), Some(&head["refs/heads/".len()..]));
            let health = worktree_health(&repo, "main").unwrap();
            assert_eq!(common_dir(&folder).unwrap(), common);
            commit_all(&folder, "task work").unwrap();
            match redirect {
                // Clean linked checkouts restore their recorded git pointers before removal.
                Redirect::GitFile | Redirect::WorktreeCommondir => {
                    assert_eq!(health[0].dirty_files, 1, "{health:?}");
                    remove_worktree(&repo, &folder, None, false).unwrap();
                    assert!(!folder.exists());
                }
                Redirect::MainCommondir => {}
            }
            assert!(!marker.exists(), "{redirect:?}: host git ran the sandbox's hook");
            unpin(&pins.work_tree);
        }
    }

    #[test]
    fn pinned_git_never_runs_in_a_submodule_the_sandbox_adds() {
        let (dir, repo) = project();
        let worktree = repo.with_file_name("app-task");
        add_worktree(&repo, &worktree, "task", "main").unwrap();
        let pins = resolve_pins(&worktree).unwrap();
        let marker = dir.path().join("hook-ran");
        // A gitlink in the worktree's index, its folder's .git naming a planted git folder,
        // and a .gitmodules asking git to look inside it.
        let evil = pins.git_dir.clone().unwrap().join("evil");
        plant_hostile_git_dir(&evil, pins.common_dir.as_ref().unwrap(), "refs/heads/main", &marker);
        std::fs::create_dir(worktree.join("sub")).unwrap();
        std::fs::write(worktree.join("sub/.git"), format!("gitdir: {}\n", evil.display())).unwrap();
        let head = git(&repo, &["rev-parse", "main"]).unwrap();
        git(&worktree, &["update-index", "--add", "--cacheinfo", &format!("160000,{},sub", head.trim())]).unwrap();
        std::fs::write(
            worktree.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n\tignore = none\n",
        )
        .unwrap();
        git(&worktree, &["status", "--porcelain"]).unwrap();
        assert!(marker.exists(), "the planted submodule must be live for this test to mean anything");
        std::fs::remove_file(&marker).unwrap();

        pin(pins.clone());
        let diff = diff_against_base(&worktree, "main", None).unwrap();
        assert!(diff.files.iter().any(|file| file.path() == "sub"), "the submodule itself still shows");
        worktree_health(&repo, "main").unwrap();
        assert!(remove_worktree(&repo, &worktree, None, false).is_err(), "untracked .gitmodules");
        assert!(!marker.exists(), "host git ran git in the sandbox's submodule");
        unpin(&pins.work_tree);
    }

    #[test]
    fn pins_refuse_worktree_config_and_stop_git_outside_a_repository() {
        let (dir, repo) = project();
        let worktree = repo.with_file_name("app-task");
        add_worktree(&repo, &worktree, "task", "main").unwrap();
        run(&repo, &["config", "extensions.worktreeConfig", "true"]);
        let error = resolve_pins(&worktree).unwrap_err();
        assert!(format!("{error:#}").contains("extensions.worktreeConfig"), "{error:#}");
        run(&repo, &["config", "extensions.worktreeConfig", "false"]);
        assert!(resolve_pins(&worktree).is_ok());

        // A folder outside any repository stays that way for host git, whatever .git appears in it.
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let pins = resolve_pins(&plain).unwrap();
        assert_eq!((pins.git_dir.as_ref(), pins.common_dir.as_ref()), (None, None));
        pin(pins.clone());
        run(&plain, &["init", "-q"]);
        assert!(git(&plain.join(".git"), &["status"]).is_err());
        assert!(diff_against_base(&plain, "main", None).is_err());
        unpin(&pins.work_tree);
        assert!(git(&plain, &["status"]).is_ok());
    }

    #[test]
    fn linux_clone_command_requires_reflinks_and_preserves_path_arguments() {
        let source = Path::new("/repo with spaces/target");
        let destination = Path::new("/new checkout/target");
        let command = linux_clone_command(source, destination);
        assert_eq!(command.get_program(), "cp");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                std::ffi::OsStr::new("-R"),
                std::ffi::OsStr::new("--reflink=always"),
                source.as_os_str(),
                destination.as_os_str()
            ]
        );
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn native_clone_skips_special_files_without_copying_a_cache() {
        let dir = crate::test_support::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("cache");
        std::fs::create_dir(&source).unwrap();
        // A socket-only tree catches GNU cp recreating special files even on a
        // filesystem that correctly rejects reflinks for ordinary files.
        let _socket = std::os::unix::net::UnixListener::bind(source.join("socket")).unwrap();
        assert!(!clone_directory(&source, &destination));
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_or_unsupported_clone_leaves_no_cache_and_never_retries_as_a_copy() {
        let dir = crate::test_support::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("dependency"), "cached dependency").unwrap();
        for failure in ["unsupported", "missing command", "partial clone"] {
            let started = std::time::Instant::now();
            assert!(!clone_directory_with(&source, &destination, |_, staged| {
                match failure {
                    "unsupported" => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "not supported")),
                    "missing command" => Command::new(dir.path().join("missing-command")).status().map(|_| ()),
                    _ => {
                        std::fs::create_dir(staged)?;
                        std::fs::write(staged.join("partial"), "partial clone")?;
                        Err(std::io::Error::other("clone failed partway through"))
                    }
                }
            }));
            assert!(started.elapsed() < std::time::Duration::from_secs(2), "{failure} delayed launch");
            assert!(!destination.exists(), "{failure} must not fall back to copying");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "partial output must be removed");
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn warm_worktree_clones_existing_folders_from_main_and_keeps_caches_independent() {
        let (_dir, repo) = project();
        let linked = repo.with_file_name("linked");
        let destination = repo.with_file_name("new");
        add_worktree(&repo, &linked, "linked", "main").unwrap();
        add_worktree(&repo, &destination, "new", "main").unwrap();
        std::fs::create_dir(repo.join("target")).unwrap();
        std::fs::write(repo.join("target/dependency"), "from main").unwrap();
        std::fs::create_dir(linked.join("target")).unwrap();
        std::fs::write(linked.join("target/dependency"), "wrong checkout").unwrap();
        std::fs::create_dir_all(repo.join("build/cache")).unwrap();
        std::fs::write(repo.join("build/cache/object"), "nested cache").unwrap();
        if !probe_cache_clone(&repo, destination.parent().unwrap()).unwrap() {
            assert!(!warm_worktree(&linked, &destination, &["target".into(), "build/cache".into()]));
            assert!(!destination.join("target").exists());
            assert!(!destination.join("build/cache").exists());
            return;
        }
        assert!(!warm_worktree(&linked, &destination, &[]));
        assert!(!destination.join("target").exists());
        let folders =
            ["missing", "a.txt", "../target", "/tmp", ".", ".git", "target", "build/cache"].map(str::to_string);
        assert!(warm_worktree(&linked, &destination, &folders));
        assert!(!destination.join("missing").exists());
        assert_eq!(std::fs::read_to_string(destination.join("target/dependency")).unwrap(), "from main");
        assert_eq!(std::fs::read_to_string(destination.join("build/cache/object")).unwrap(), "nested cache");
        std::fs::write(destination.join("target/dependency"), "agent build").unwrap();
        assert!(!warm_worktree(&repo, &destination, &folders));
        assert_eq!(std::fs::read_to_string(destination.join("target/dependency")).unwrap(), "agent build");
        assert_eq!(std::fs::read_to_string(repo.join("target/dependency")).unwrap(), "from main");
        assert!(probe_cache_clone(&repo, destination.parent().unwrap()).unwrap());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_native_reflink_clones_independently_or_skips_cleanly() {
        use std::os::unix::fs::{MetadataExt, symlink};

        // The Linux harness can repeat this test on ext4, btrfs and XFS mounts.
        let dir = match std::env::var_os("IMACS_COW_TEST_DIR") {
            Some(parent) => tempfile::tempdir_in(parent).unwrap(),
            None => crate::test_support::tempdir().unwrap(),
        };
        let source = dir.path().join("source");
        let destination = dir.path().join("cache");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("nested/data"), vec![b'+'; 8192]).unwrap();
        symlink("nested/data", source.join("link")).unwrap();
        let supported = probe_cache_clone(&source, dir.path()).unwrap();
        assert_eq!(clone_directory(&source, &destination), supported);
        println!("Linux native cache clone supported on {}: {supported}", dir.path().display());
        if supported {
            assert_eq!(std::fs::read(destination.join("nested/data")).unwrap(), vec![b'+'; 8192]);
            assert_eq!(std::fs::read_link(destination.join("link")).unwrap(), Path::new("nested/data"));
            assert_ne!(
                std::fs::metadata(source.join("nested/data")).unwrap().ino(),
                std::fs::metadata(destination.join("nested/data")).unwrap().ino()
            );
            std::fs::write(destination.join("nested/data"), "agent build").unwrap();
            assert_eq!(std::fs::read(source.join("nested/data")).unwrap(), vec![b'+'; 8192]);
        } else {
            assert!(!destination.exists());
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                1,
                "failed clones must leave no staging directory"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn warm_worktree_does_not_follow_cache_symlinks_outside_checkouts() {
        let (dir, repo) = project();
        let destination = repo.with_file_name("new");
        add_worktree(&repo, &destination, "new", "main").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("target")).unwrap();
        assert!(!warm_worktree(&repo, &destination, &["target".into()]));
        std::fs::create_dir_all(repo.join("build/cache")).unwrap();
        std::os::unix::fs::symlink(&outside, destination.join("build")).unwrap();
        assert!(!warm_worktree(&repo, &destination, &["build/cache".into()]));
        assert!(!outside.join("cache").exists());
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
    fn links_in_a_worktree_show_their_target_and_are_never_read_through() {
        use std::os::unix::fs::symlink;
        let (dir, repo) = project();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), "host credentials\n").unwrap();
        symlink(outside.join("secret"), repo.join("link")).unwrap();
        symlink(&outside, repo.join("folder-link")).unwrap();
        // A FIFO must neither show nor keep the diff waiting for a writer.
        assert!(Command::new("mkfifo").arg(repo.join("fifo")).status().unwrap().success());

        let diff = diff_against_base(&repo, "main", None).unwrap();
        let lines = |path: &str| -> Vec<String> {
            let file = diff.files.iter().find(|f| f.path() == path).unwrap_or_else(|| panic!("{path}: {diff:?}"));
            file.hunks.iter().flat_map(|h| &h.lines).map(|l| l.text.clone()).collect()
        };
        assert_eq!(lines("link"), [outside.join("secret").display().to_string()]);
        assert_eq!(lines("folder-link"), [outside.display().to_string()]);
        assert!(!diff.files.iter().any(|f| f.path() == "fifo"));
        let shown = format!("{diff:?}");
        assert!(!shown.contains("host credentials"), "{shown}");

        // The file viewer reads with the same rule, also for a folder replaced by a link.
        let read = |path: &str, limit| read_worktree_file(&repo, path, limit);
        assert_eq!(read("link", 1024), Some(WorktreeFile::Link(outside.join("secret").display().to_string())));
        assert_eq!(
            read("a.txt", 1024),
            Some(WorktreeFile::File { len: 14, bytes: Some(b"one\ntwo\nthree\n".to_vec()) })
        );
        assert_eq!(read("a.txt", 4), Some(WorktreeFile::File { len: 14, bytes: None }));
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        assert!(read("src/main.rs", 1024).is_some());
        std::fs::remove_dir_all(repo.join("src")).unwrap();
        std::fs::write(outside.join("main.rs"), "host file\n").unwrap();
        symlink(&outside, repo.join("src")).unwrap();
        for path in ["src/main.rs", "folder-link/secret", "fifo", "../outside/secret", "src"] {
            assert!(!matches!(read(path, 1024), Some(WorktreeFile::File { .. })), "{path}");
        }
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
        let dir = crate::test_support::tempdir().unwrap();
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
