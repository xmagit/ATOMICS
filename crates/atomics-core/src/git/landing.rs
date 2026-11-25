//! Integration admission and landing use host-recorded worktrees and immutable commits.
use super::*;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;

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

/// Only advance the named target from the commit inspected before verification.
pub fn land_fast_forward(repo: &Path, target: &str, expected: &str, verified: &str) -> Result<()> {
    let lock = worktree_lock(repo)?;
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    let common = common_dir(repo)?;
    let _exclusive = landing_lock(&common)?;
    let recovered = recover(repo, &common)?;
    ensure!(git(repo, &["check-ref-format", "--branch", target]).is_ok(), "invalid landing target");
    let reference = format!("refs/heads/{target}");
    ensure!(
        git(repo, &["symbolic-ref", "--quiet", &reference]).is_err(),
        "landing target must be a direct local branch, not a symbolic ref"
    );
    if recovered.is_some_and(|intent| intent.reference == reference && intent.old == expected && intent.new == verified)
        && commit_of(repo, &reference)?.as_deref() == Some(verified)
    {
        return Ok(());
    }
    ensure!(
        commit_of(repo, &reference)?.as_deref() == Some(expected),
        "landing target moved during verification; verify again"
    );
    ensure!(expected != verified, "landing target already points to the verified commit");
    ensure!(
        is_ancestor(repo, expected, verified)? == Some(true),
        "landing target cannot be fast-forwarded; merge it into the integration branch and verify again"
    );
    let trees: Vec<_> =
        list_worktrees(repo)?.into_iter().filter(|tree| tree.branch.as_deref() == Some(target)).collect();
    ensure!(trees.len() <= 1, "landing target has multiple worktree claims");
    if let Some(tree) = trees.first() {
        ensure!(tree.trusted, "landing target has an untrusted worktree");
        advance_checkout(&common, &tree.path, &reference, expected, verified)?;
    } else {
        git(repo, &["update-ref", "--no-deref", &reference, verified, expected])?;
    }
    ensure!(commit_of(repo, &reference)?.as_deref() == Some(verified), "landing target changed before completion");
    Ok(())
}

const INTENT: &str = "atomics-landing-intent";

#[derive(Serialize, Deserialize)]
struct LandingIntent {
    path: PathBuf,
    checkout_id: (u64, u64),
    index: PathBuf,
    admin_id: (u64, u64),
    lock_id: (u64, u64),
    reference: String,
    old: String,
    new: String,
}

fn file_id(path: &Path) -> Result<(u64, u64)> {
    let m = path.symlink_metadata()?;
    Ok((m.dev(), m.ino()))
}

fn landing_lock(common: &Path) -> Result<File> {
    let file = File::open(common)?;
    // A process forked elsewhere in ATOMICS holds a copy of a just-released lock's descriptor until it
    // execs, so a moment of contention is not another landing. A real one holds the lock far longer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("another landing is in progress"),
        }
    }
}

/// Reconcile an interrupted publication before selecting a base or admitting a hook.
pub fn reconcile_landing(repo: &Path) -> Result<()> {
    let lock = worktree_lock(repo)?;
    let _turn = lock.lock().unwrap_or_else(|e| e.into_inner());
    let common = common_dir(repo)?;
    let _exclusive = landing_lock(&common)?;
    recover(repo, &common).map(drop)
}

fn recover(repo: &Path, common: &Path) -> Result<Option<LandingIntent>> {
    let journal = common.join(INTENT);
    if !journal.try_exists()? {
        return Ok(None);
    }
    ensure!(journal.symlink_metadata()?.is_dir(), "invalid landing intent directory");
    let intent: LandingIntent = serde_json::from_str(&read_regular_file(&journal.join("intent"), 32 * 1024)?)?;
    ensure!(
        !sandbox_writable(&intent.path)
            && intent.reference.starts_with("refs/heads/")
            && git(repo, &["check-ref-format", &intent.reference]).is_ok()
            && [&intent.old, &intent.new]
                .iter()
                .all(|oid| matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()))
            && file_id(&journal.join("lock-owner"))? == intent.lock_id
            && file_id(&intent.path)? == intent.checkout_id
            && common_dir(&intent.path)? == common
            && index_file(&intent.path)? == intent.index
            && file_id(intent.index.parent().context("invalid landing index")?)? == intent.admin_id,
        "pending landing checkout identity changed"
    );
    ensure!(
        git(repo, &["symbolic-ref", "--quiet", &intent.reference]).is_err(),
        "pending landing target became symbolic"
    );
    let tip = commit_of(repo, &intent.reference)?;
    let advanced = tip.as_deref() == Some(&intent.new);
    ensure!(advanced || tip.as_deref() == Some(&intent.old), "pending landing target has moved; inspect its checkout");
    let locked = intent.owns_index_lock()?;
    if !locked && advanced {
        // The index and checkout were already published and our lock removed.
        ensure!(
            git(&intent.path, &["status", "--porcelain", "--untracked-files=no"])?.is_empty(),
            "pending landing checkout changed after its index lock was released"
        );
    }
    if advanced {
        ensure!(
            git(&intent.path, &["symbolic-ref", "--quiet", "HEAD"])?.trim() == intent.reference,
            "pending landing worktree switched branches"
        );
        if locked {
            intent.reconcile_checkout(&journal, &intent.new)?;
        }
    }
    intent.finish(&journal)?;
    Ok(Some(intent))
}

impl LandingIntent {
    fn owns_index_lock(&self) -> Result<bool> {
        match self.index.with_added_extension("lock").symlink_metadata() {
            Ok(m) => {
                ensure!((m.dev(), m.ino()) == self.lock_id, "landing index lock belongs to another writer");
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn indexed(&self, index: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
        let mut cmd = command(&self.path, args)?;
        cmd.env("GIT_INDEX_FILE", index);
        run(cmd, args, input, SLOW)
    }

    /// Start with the pre-landing index. Adopt paths already at their destination
    /// after a partial checkout, then use Git's ordinary two-tree merge to finish.
    /// Its local-change and untracked-file checks also apply on recovery.
    fn reconcile_checkout(&self, journal: &Path, destination: &str) -> Result<()> {
        let scratch = tempfile::tempdir_in(journal)?;
        let index = scratch.path().join("index");
        fs::copy(journal.join("before"), &index)?;
        let probe = scratch.path().join("new-index");
        self.indexed(&probe, &["read-tree", &self.new], None)?;
        let changed = |index: &Path, commit: &str| -> Result<HashSet<Vec<u8>>> {
            let args = ["diff", "--name-only", "--no-renames", "-z", commit, "--"];
            let mut cmd = command(&self.path, &args)?;
            cmd.env("GIT_INDEX_FILE", index);
            let diff = output(cmd, &args, None, SLOW)?;
            ensure!(diff.status.success(), "cannot inspect interrupted checkout");
            Ok(diff.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()).map(Vec::from).collect())
        };
        let old_edits = changed(&index, &self.old)?;
        let new_edits = changed(&probe, &self.new)?;
        let args =
            ["diff-tree", "-r", "--no-commit-id", "--no-renames", "--raw", "--no-abbrev", "-z", &self.old, &self.new];
        let raw = output(command(&self.path, &args)?, &args, None, SLOW)?;
        ensure!(raw.status.success(), "cannot inspect landing trees");
        let mut fields = raw.stdout.split(|&b| b == 0).filter(|v| !v.is_empty());
        let mut changes = Vec::new();
        while let Some(header) = fields.next() {
            let path = fields.next().context("missing landing path")?;
            let parts: Vec<_> = std::str::from_utf8(header)?.split_whitespace().collect();
            let [old_mode, mode, _, object, _] = parts.as_slice() else { bail!("invalid Git tree change") };
            let old_mode = old_mode.strip_prefix(':').context("invalid Git tree mode")?;
            // A diff against a tree has no entry for an untracked path. Check
            // absence explicitly on the side of an addition or deletion.
            let matches = |mode: &str, edits: &HashSet<Vec<u8>>| -> Result<bool> {
                use std::os::unix::ffi::OsStrExt;
                if mode != "000000" {
                    return Ok(!edits.contains(path));
                }
                match self.path.join(std::ffi::OsStr::from_bytes(path)).symlink_metadata() {
                    Ok(m) => Ok(m.is_dir()),
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                        Ok(true)
                    }
                    Err(e) => Err(e.into()),
                }
            };
            if !matches(mode, &new_edits)? {
                ensure!(
                    matches(old_mode, &old_edits)?,
                    "landing path {} changed during checkout; recovery pending",
                    String::from_utf8_lossy(path)
                );
                continue;
            }
            changes.extend_from_slice(format!("{mode} {object}\t").as_bytes());
            changes.extend_from_slice(path);
            changes.push(0);
        }
        if !changes.is_empty() {
            self.indexed(&index, &["update-index", "-z", "--index-info"], Some(&changes))?;
        }
        let partial = self.indexed(&index, &["write-tree"], None)?;
        self.indexed(&index, &["read-tree", "-u", "-m", partial.trim(), destination], None)?;
        self.publish_index(&index)
    }

    fn publish_index(&self, index: &Path) -> Result<()> {
        ensure!(self.owns_index_lock()?, "landing index lock disappeared");
        let parent = self.index.parent().context("invalid landing index")?;
        let mut output = tempfile::NamedTempFile::new_in(parent)?;
        let mut source = File::open(index)?;
        std::io::copy(&mut source, &mut output)?;
        let metadata = source.metadata()?;
        output.as_file().set_permissions(metadata.permissions())?;
        output.as_file().set_modified(metadata.modified()?)?;
        output.as_file().sync_all()?;
        output.persist(&self.index)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn finish(&self, journal: &Path) -> Result<()> {
        if self.owns_index_lock()? {
            fs::remove_file(self.index.with_added_extension("lock"))?;
        }
        // Unpublish the complete journal atomically before deleting its files.
        let retired = tempfile::tempdir_in(journal.parent().unwrap())?;
        fs::rename(journal, retired.path().join("finished"))?;
        File::open(journal.parent().unwrap())?.sync_all()?;
        Ok(())
    }
}

fn advance_checkout(common: &Path, path: &Path, reference: &str, expected: &str, verified: &str) -> Result<()> {
    ensure!(!sandbox_writable(path), "landing target checkout is writable by a sandbox");
    let index = index_file(path)?;
    let lock = index.with_added_extension("lock");
    let prepared = tempfile::tempdir_in(common)?;
    let marker = prepared.path().join("lock-owner");
    File::create_new(&marker)?.sync_all()?;
    let intent = LandingIntent {
        path: path.to_owned(),
        checkout_id: file_id(path)?,
        admin_id: file_id(index.parent().unwrap())?,
        index,
        lock_id: file_id(&marker)?,
        reference: reference.into(),
        old: expected.into(),
        new: verified.into(),
    };
    let record = File::create_new(prepared.path().join("intent"))?;
    serde_json::to_writer(&record, &intent)?;
    record.sync_all()?;
    File::open(prepared.path())?.sync_all()?;
    let journal = common.join(INTENT);
    fs::rename(prepared.path(), &journal)?;
    File::open(common)?.sync_all()?;
    if let Err(err) = fs::hard_link(journal.join("lock-owner"), &lock) {
        fs::remove_dir_all(&journal)?;
        return Err(err).context("could not lock the landing worktree index");
    }
    let before = journal.join("before");
    let next = journal.join("next");
    let publish = (|| -> Result<()> {
        ensure!(
            git(path, &["symbolic-ref", "--quiet", "HEAD"])?.trim() == reference,
            "landing worktree switched branches"
        );
        ensure!(git(path, &["status", "--porcelain"])?.is_empty(), "landing target worktree is dirty");
        if intent.index.try_exists()? {
            fs::copy(&intent.index, &before)?;
            // The status above could not write what it learned while landing holds the index, and a merge of two
            // trees refuses a file whose index entry is only out of date.
            intent.indexed(&before, &["update-index", "-q", "--refresh"], None)?;
        } else {
            intent.indexed(&before, &["read-tree", "--empty"], None)?;
        }
        File::open(&before)?.sync_all()?;
        fs::copy(&before, &next)?;
        intent.indexed(&next, &["read-tree", "-n", "-u", "-m", expected, verified], None)?;
        File::open(&journal)?.sync_all()?;
        git(path, &["update-ref", "--no-deref", reference, verified, expected])?;
        Ok(())
    })();
    if let Err(error) = publish {
        // An interrupted Git process may have committed its ref transaction.
        if commit_of(path, reference)?.as_deref() == Some(expected) {
            intent.finish(&journal)?;
        }
        return Err(error);
    }
    #[cfg(test)]
    if std::env::var_os("ATOMICS_TEST_INTERRUPT_LANDING").is_some() {
        std::process::exit(77);
    }
    let checkout = intent
        .indexed(&next, &["read-tree", "-u", "-m", expected, verified], None)
        .and_then(|_| intent.publish_index(&next));
    if let Err(error) = checkout {
        intent
            .reconcile_checkout(&journal, expected)
            .with_context(|| format!("checkout failed ({error:#}); landing recovery pending"))?;
        git(path, &["update-ref", "--no-deref", reference, expected, verified])?;
        intent.finish(&journal)?;
        return Err(error).context("could not update the landing worktree");
    }
    ensure!(
        commit_of(path, reference)?.as_deref() == Some(verified),
        "landing target changed during checkout; recovery pending"
    );
    intent.finish(&journal)
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
    fn interrupted_landing_recovers_the_checkout_and_releases_its_lock() {
        const CHILD: &str = "ATOMICS_TEST_INTERRUPT_LANDING";
        if let Some(path) = std::env::var_os(CHILD) {
            let repo = Path::new(&path);
            let old = commit_of(repo, "main").unwrap().unwrap();
            let new = commit_of(repo, "feature").unwrap().unwrap();
            land_fast_forward(repo, "main", &old, &new).unwrap();
            panic!("expected interruption");
        }
        let (dir, repo) = project();
        let task = dir.path().join("feature");
        add_worktree(&repo, &task, "feature", "main").unwrap();
        std::fs::write(task.join("a.txt"), "landed").unwrap();
        std::fs::write(task.join("added"), "new file").unwrap();
        commit_all(&task, "Feature").unwrap();
        let old = commit_of(&repo, "main").unwrap().unwrap();
        let new = commit_of(&repo, "feature").unwrap().unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "git::landing::tests::interrupted_landing_recovers_the_checkout_and_releases_its_lock"])
            .env(CHILD, &repo)
            .status()
            .unwrap();
        assert_eq!(child.code(), Some(77));
        assert_eq!(commit_of(&repo, "main").unwrap().as_deref(), Some(new.as_str()));
        assert_ne!(std::fs::read(repo.join("a.txt")).unwrap(), b"landed");
        // Recovery must also resume after Git wrote one file, and must leave a
        // replacement index.lock alone even if the branch still has the right tip.
        std::fs::write(repo.join("added"), "new file").unwrap();
        let lock = index_file(&repo).unwrap().with_added_extension("lock");
        let held = dir.path().join("held");
        fs::rename(&lock, &held).unwrap();
        fs::write(&lock, b"another writer").unwrap();
        assert!(reconcile_landing(&repo).is_err());
        assert_eq!(fs::read(&lock).unwrap(), b"another writer");
        fs::rename(held, &lock).unwrap();
        land_fast_forward(&repo, "main", &old, &new).unwrap();
        assert_eq!(std::fs::read(repo.join("a.txt")).unwrap(), b"landed");
        assert!(!index_file(&repo).unwrap().with_added_extension("lock").exists());
        assert!(git(&repo, &["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn landing_passes_over_an_index_that_is_only_out_of_date() {
        let (dir, repo) = project();
        std::fs::write(repo.join("shared.txt"), "base").unwrap();
        commit_all(&repo, "base").unwrap();
        let before = commit_of(&repo, "HEAD").unwrap().unwrap();
        let task = dir.path().join("task");
        add_worktree(&repo, &task, "task", "main").unwrap();
        std::fs::write(task.join("shared.txt"), "landed").unwrap();
        commit_all(&task, "feature").unwrap();
        let verified = commit_of(&task, "HEAD").unwrap().unwrap();
        // Written again as it was: the index entry no longer fits the file, whose content is the same.
        std::fs::write(repo.join("shared.txt"), "base").unwrap();
        let file = File::options().write(true).open(repo.join("shared.txt")).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)).unwrap();
        land_fast_forward(&repo, "main", &before, &verified).unwrap();
        assert_eq!(std::fs::read(repo.join("shared.txt")).unwrap(), b"landed");
        assert!(git(&repo, &["status", "--porcelain"]).unwrap().is_empty());
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
        let lock = index_file(&repo).unwrap().with_added_extension("lock");
        std::fs::write(&lock, "other writer").unwrap();
        assert!(land_fast_forward(&repo, "main", &before, &verified).is_err());
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "other writer");
        assert_eq!(commit_of(&repo, "main").unwrap().as_deref(), Some(before.as_str()));
        std::fs::remove_file(lock).unwrap();
        land_fast_forward(&repo, "main", &before, &verified).unwrap();
        assert!(repo.join("feature.txt").exists());
        assert!(git(&repo, &["status", "--porcelain"]).unwrap().is_empty());
        assert!(land_fast_forward(&repo, "main", &verified, &verified).is_err());
        assert!(land_fast_forward(&repo, "main", &before, &verified).unwrap_err().to_string().contains("moved"));
        assert!(land_fast_forward(&repo, "main", &verified, &before).unwrap_err().to_string().contains("fast-forward"));
    }
}
