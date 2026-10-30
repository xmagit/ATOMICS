//! Host-side recovery for the shared refs a sandbox can write. This detects changes after a
//! turn; it is not an access-control boundary or proof of which writer changed a shared ref.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use imacs_core::git;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum RefValue {
    Object(String),
    Symbolic(String),
}

#[derive(Serialize, Deserialize)]
pub struct RefGuard {
    pub common: PathBuf,
    pub branch: Option<String>,
    refs: BTreeMap<String, RefValue>,
    #[serde(skip)]
    cwd: PathBuf,
    #[serde(skip)]
    file: PathBuf,
}

impl RefGuard {
    /// Load the original snapshot on reattach, before the agent gets another turn. The file
    /// lives outside every VM mount, so an agent cannot replace its own baseline.
    pub fn open(cwd: &Path, file: PathBuf) -> Result<Option<Self>> {
        if git::worktree_of(cwd).is_none() {
            return Ok(None);
        }
        let common = git::git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let common = PathBuf::from(common.trim()).canonicalize()?;
        let mut guard = if file.exists() {
            let guard: Self = serde_json::from_slice(&std::fs::read(&file)?)?;
            if guard.common != common {
                bail!("ref guard snapshot belongs to another repository");
            }
            guard
        } else {
            Self {
                common,
                branch: git::git(cwd, &["symbolic-ref", "-q", "HEAD"]).ok().map(|s| s.trim().to_string()),
                refs: read_refs(cwd)?,
                cwd: cwd.into(),
                file: file.clone(),
            }
        };
        guard.cwd = cwd.into();
        guard.file = file;
        guard.save()?;
        Ok(Some(guard))
    }

    /// Restore changed, deleted and newly created refs outside the task branches. Other
    /// active tasks' branches must be exempt too: they share this same refs directory.
    pub fn check(&mut self, concurrent: &BTreeSet<String>) -> Result<Vec<String>> {
        let current = read_refs(&self.cwd)?;
        let names: BTreeSet<_> = self.refs.keys().chain(current.keys()).cloned().collect();
        let mut alerts = Vec::new();
        for name in names {
            if self.branch.as_ref() == Some(&name) || concurrent.contains(&name) {
                match current.get(&name) {
                    Some(value) => {
                        self.refs.insert(name, value.clone());
                    }
                    None => {
                        self.refs.remove(&name);
                    }
                }
                continue;
            }
            let (before, after) = (self.refs.get(&name), current.get(&name));
            if before == after {
                continue;
            }
            let result = restore(&self.cwd, &name, before, after);
            alerts.push(match result {
                Ok(()) => format!("Restored {name}: {after:?} → {before:?}"),
                Err(error) => format!("RESTORE FAILED for {name}: {error:#}; expected {before:?}, found {after:?}"),
            });
        }
        // Do not change the pinned branch if the agent switches HEAD to another task's branch.
        let head = git::git(&self.cwd, &["symbolic-ref", "-q", "HEAD"]).ok().map(|s| s.trim().to_string());
        if head != self.branch {
            alerts.push(format!("Worktree HEAD changed: {head:?}; task branch is {:?}", self.branch));
            if let Some(branch) = &self.branch {
                git::git(&self.cwd, &["symbolic-ref", "HEAD", branch]).context("restoring task HEAD")?;
            }
        }
        self.save()?;
        Ok(alerts)
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(self.file.parent().context("ref guard snapshot has no parent")?)?;
        let temporary = self.file.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, &self.file)?;
        Ok(())
    }
}

/// A session's guard, checked on [`Checks`]' thread: whose refs it guards is known without
/// waiting for a check in progress.
#[derive(Clone)]
pub struct Shared {
    pub common: PathBuf,
    pub branch: Option<String>,
    guard: Arc<Mutex<RefGuard>>,
}

impl Shared {
    pub fn new(guard: RefGuard) -> Self {
        Self { common: guard.common.clone(), branch: guard.branch.clone(), guard: Arc::new(Mutex::new(guard)) }
    }

    /// Runs git: call it on [`Checks`]' thread.
    pub fn check(&self, exempt: &Exempt) -> Result<Vec<String>> {
        let concurrent = exempt.resolve(&self.common);
        self.guard.lock().unwrap_or_else(|e| e.into_inner()).check(&concurrent)
    }
}

/// Branches whose moves a check accepts, gathered where git must not run: each with its
/// repository's common dir, or with a folder whose repository [`Exempt::resolve`] asks git for.
#[derive(Clone, Debug, Default)]
pub struct Exempt {
    pub refs: Vec<(PathBuf, String)>,
    pub folders: Vec<(PathBuf, String)>,
}

impl Exempt {
    /// The exempt refs in the repository at `common`. Runs git.
    pub fn resolve(&self, common: &Path) -> BTreeSet<String> {
        let mut commons: HashMap<&Path, Option<PathBuf>> = HashMap::new();
        let mut refs: BTreeSet<String> =
            self.refs.iter().filter(|(other, _)| other == common).map(|(_, name)| name.clone()).collect();
        for (folder, name) in &self.folders {
            if !refs.contains(name)
                && commons.entry(folder).or_insert_with(|| git::common_dir(folder).ok()).as_deref() == Some(common)
            {
                refs.insert(name.clone());
            }
        }
        refs
    }
}

/// Ref guard checks run git, which the state lock, and so the UI, must never wait for. They
/// run on one thread, in the order they were asked for: a check that accepts an agent's last
/// commits must never be overtaken by a later one that no longer exempts its branch.
#[derive(Clone)]
pub struct Checks {
    jobs: Sender<Box<dyn FnOnce() + Send>>,
}

impl Checks {
    pub fn new() -> Self {
        let (jobs, rx) = channel::<Box<dyn FnOnce() + Send>>();
        std::thread::Builder::new()
            .name("imacs-ref-guard".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job();
                }
            })
            .expect("spawn ref guard thread");
        Self { jobs }
    }

    pub fn run(&self, job: impl FnOnce() + Send + 'static) {
        let _ = self.jobs.send(Box::new(job));
    }
}

/// A sandboxed session's guard, if its folder is in a repository, and what its first check found.
pub type Opened = Option<(Shared, Vec<String>)>;

/// Opens a sandboxed session's guard and checks it once before its agent gets a turn, on
/// [`Checks`]' thread. Blocks: call it off the state lock.
pub fn open_and_check(checks: &Checks, cwd: &Path, file: PathBuf, exempt: Exempt) -> Result<Opened, String> {
    let Some(guard) = RefGuard::open(cwd, file).map_err(|error| format!("Could not start ref guard: {error:#}"))?
    else {
        return Ok(None);
    };
    let (guard, (done, result)) = (Shared::new(guard), channel());
    let checked = guard.clone();
    checks.run(move || {
        let _ = done.send(checked.check(&exempt));
    });
    let alerts = result
        .recv()
        .map_err(|_| "ref guard thread stopped".to_string())?
        .map_err(|error| format!("Could not check refs before starting: {error:#}"))?;
    Ok(Some((guard, alerts)))
}

fn read_refs(cwd: &Path) -> Result<BTreeMap<String, RefValue>> {
    let output = git::git(cwd, &["for-each-ref", "--format=%(refname)%09%(objectname)%09%(symref)"])?;
    let mut refs = BTreeMap::new();
    for line in output.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        let [name, object, symbolic] = fields[..] else { bail!("unexpected git ref: {line}") };
        refs.insert(
            name.into(),
            if symbolic.is_empty() { RefValue::Object(object.into()) } else { RefValue::Symbolic(symbolic.into()) },
        );
    }
    // for-each-ref omits dangling symbolic refs. Read loose refs too so creating one cannot
    // evade comparison (or turn a deleted protected ref into an invisible symbolic ref).
    let common = git::git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    read_loose(&PathBuf::from(common.trim()).join("refs"), "refs", &mut refs)?;
    Ok(refs)
}

fn read_loose(dir: &Path, prefix: &str, refs: &mut BTreeMap<String, RefValue>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        if name.ends_with(".lock") {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            read_loose(&entry.path(), &name, refs)?;
        } else if kind.is_file() {
            let value = std::fs::read_to_string(entry.path())?;
            let value = value.trim();
            let value = value
                .strip_prefix("ref: ")
                .map_or_else(|| RefValue::Object(value.into()), |target| RefValue::Symbolic(target.into()));
            refs.insert(name, value);
        } else {
            bail!("ref {name} is not a regular file or folder");
        }
    }
    Ok(())
}

/// Compare-and-swap under Git's ref lock. `no-deref` prevents a malicious symbolic ref from
/// making restoration write through to a different branch.
fn restore(cwd: &Path, name: &str, before: Option<&RefValue>, after: Option<&RefValue>) -> Result<()> {
    // Older Git versions have no symbolic-ref transactions. Recheck before those operations;
    // ordinary branch/tag OIDs below use Git's atomic old-value check.
    if matches!(before, Some(RefValue::Symbolic(_))) || matches!(after, Some(RefValue::Symbolic(_))) {
        if read_refs(cwd)?.get(name) != after {
            bail!("ref changed again during recovery");
        }
        match before {
            Some(RefValue::Symbolic(target)) => {
                git::git(cwd, &["symbolic-ref", name, target])?;
            }
            Some(RefValue::Object(object)) => {
                git::git(cwd, &["update-ref", "--no-deref", name, object])?;
            }
            None => {
                git::git(cwd, &["update-ref", "--no-deref", "-d", name])?;
            }
        }
        return Ok(());
    }
    let operation = match (before, after) {
        (Some(RefValue::Object(object)), Some(RefValue::Object(old))) => format!("update {name} {object} {old}"),
        (Some(RefValue::Object(object)), None) => format!("create {name} {object}"),
        (None, Some(RefValue::Object(old))) => format!("delete {name} {old}"),
        (None, None) => return Ok(()),
        _ => unreachable!("symbolic refs handled above"),
    };
    let mut child = git::command(cwd, &["update-ref", "--stdin"])?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("opening git update-ref stdin")?
        .write_all(format!("start\noption no-deref\n{operation}\nprepare\ncommit\n").as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let worktree = dir.path().join("task");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            &["init", "-qb", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.org"],
            &["commit", "--allow-empty", "-qm", "initial"],
        ] {
            git::git(&repo, args).unwrap();
        }
        git::add_worktree(&repo, &worktree, "task", "main").unwrap();
        (dir, repo, worktree)
    }

    #[test]
    fn restores_moved_deleted_created_packed_and_symbolic_refs_after_reattach() {
        let (dir, repo, worktree) = repository();
        let initial = git::git(&repo, &["rev-parse", "main"]).unwrap().trim().to_string();
        git::git(&repo, &["branch", "other"]).unwrap();
        git::git(&repo, &["tag", "original"]).unwrap();
        git::git(&repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/heads/main"]).unwrap();
        git::git(&repo, &["pack-refs", "--all"]).unwrap();
        let file = dir.path().join("host-only/snapshot.json");
        let guard = RefGuard::open(&worktree, file.clone()).unwrap().unwrap();
        drop(guard);
        git::git(&worktree, &["commit", "--allow-empty", "-qm", "task commit"]).unwrap();
        let task = git::git(&worktree, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        git::git(&worktree, &["update-ref", "refs/heads/main", &task]).unwrap();
        git::git(&worktree, &["update-ref", "-d", "refs/heads/other"]).unwrap();
        git::git(&worktree, &["tag", "injected"]).unwrap();
        git::git(&worktree, &["symbolic-ref", "refs/tags/original", "refs/heads/task"]).unwrap();
        git::git(&worktree, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/heads/task"]).unwrap();
        git::git(&worktree, &["symbolic-ref", "refs/heads/dangling", "refs/heads/missing"]).unwrap();
        let mut guard = RefGuard::open(&worktree, file).unwrap().unwrap();
        let alerts = guard.check(&BTreeSet::new()).unwrap();
        assert_eq!(alerts.len(), 6, "{alerts:?}");
        assert!(alerts.iter().all(|alert| alert.starts_with("Restored")), "{alerts:?}");
        for name in ["main", "other", "original", "origin/HEAD"] {
            assert_eq!(git::git(&repo, &["rev-parse", name]).unwrap().trim(), initial, "{name}");
        }
        assert_eq!(git::git(&repo, &["rev-parse", "task"]).unwrap().trim(), task);
        assert!(git::git(&repo, &["rev-parse", "--verify", "refs/tags/injected"]).is_err());
        assert!(git::git(&repo, &["symbolic-ref", "refs/heads/dangling"]).is_err());
        assert!(guard.check(&BTreeSet::new()).unwrap().is_empty());
    }

    #[test]
    fn concurrent_task_commits_are_accepted_and_failed_restores_keep_the_original_baseline() {
        let (dir, repo, worktree) = repository();
        git::git(&repo, &["branch", "peer"]).unwrap();
        let mut guard = RefGuard::open(&worktree, dir.path().join("snapshot.json")).unwrap().unwrap();
        let initial = git::git(&repo, &["rev-parse", "main"]).unwrap();
        git::git(&worktree, &["commit", "--allow-empty", "-qm", "advance"]).unwrap();
        let task = git::git(&worktree, &["rev-parse", "HEAD"]).unwrap();
        git::git(&worktree, &["update-ref", "refs/heads/peer", task.trim()]).unwrap();
        assert!(guard.check(&BTreeSet::from(["refs/heads/peer".into()])).unwrap().is_empty());
        assert!(guard.check(&BTreeSet::new()).unwrap().is_empty(), "accepted commit survives peer exit");
        git::git(&repo, &["update-ref", "refs/heads/main", task.trim()]).unwrap();
        let lock = repo.join(".git/refs/heads/main.lock");
        std::fs::write(&lock, "").unwrap();
        let alerts = guard.check(&BTreeSet::new()).unwrap();
        assert!(alerts[0].contains("RESTORE FAILED"), "{alerts:?}");
        std::fs::remove_file(lock).unwrap();
        assert!(guard.check(&BTreeSet::new()).unwrap()[0].starts_with("Restored"));
        assert_eq!(git::git(&repo, &["rev-parse", "main"]).unwrap(), initial);
    }
}
