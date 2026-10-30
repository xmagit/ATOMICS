//! Host-side recovery for the shared refs a sandbox can write. This detects changes after a
//! turn; it is not an access-control boundary or proof of which writer changed a shared ref.
//!
//! A check keeps what this machine wrote since the last one, and puts back the rest:
//! - refs IMACS wrote itself ([`host_write`]), at the values IMACS left;
//! - a branch a checkout on this machine moved, at the commit that checkout's HEAD log left it at:
//!   the main checkout, or a linked worktree no sandbox works in;
//! - remote-tracking refs and tags at an object a fetch on this machine wrote to its FETCH_HEAD,
//!   and those such a fetch may have pruned;
//! - `refs/stash`, which every checkout shares, and the quarantine below are not guarded.
//!
//! Those traces are in git folders no sandbox mounts, except the main checkout's HEAD log, which is
//! in the shared `logs` folder. Putting a ref back never deletes a value: one it replaces is kept at
//! `refs/imacs/quarantine/<sandbox>/<ref>@<object>`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use imacs_core::{git, paths};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum RefValue {
    Object(String),
    Symbolic(String),
}

/// A ref's value as an alert shows it: a short object id or a symbolic target, or absent.
fn shown(value: Option<&RefValue>) -> String {
    match value {
        Some(RefValue::Object(object)) => object.chars().take(12).collect(),
        Some(RefValue::Symbolic(target)) => format!("a link to {target}"),
        None => "absent".into(),
    }
}

/// Refs no check puts back: the stash every checkout shares, and the values checks keep aside.
const UNGUARDED: [&str; 2] = ["refs/stash", QUARANTINE];
const QUARANTINE: &str = "refs/imacs/quarantine/";

/// Whether `pattern`, a ref name or a prefix ending in `/`, covers the ref `name`.
fn covers(pattern: &str, name: &str) -> bool {
    if pattern.ends_with('/') { name.starts_with(pattern) } else { name == pattern }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Refs IMACS itself writes on this machine, by repository folder: an integration's merge, a new
/// worktree's branch, a cleaned-up branch, a fetch's remote-tracking refs. A guard whose snapshot is
/// older must neither undo nor restore them, and its check may have been queued before IMACS wrote them.
struct HostWrites {
    /// Writes in progress, by ref name or prefix: every check accepts these refs.
    pending: Vec<(PathBuf, String)>,
    /// The refs each write left under its name or prefix; one it deleted is absent. A check accepts
    /// a ref still at that value and restores any other, such as an agent's.
    written: Vec<(PathBuf, String, BTreeMap<String, RefValue>)>,
}

static HOST_WRITES: Mutex<HostWrites> = Mutex::new(HostWrites { pending: Vec::new(), written: Vec::new() });

/// Marks `refs/heads/<branch>` in `repo`'s repository as written by IMACS until the returned
/// [`HostWrite`] drops, which records the values the write left. Call it before the write starts.
pub fn host_write<S: AsRef<str>>(repo: &Path, branches: impl IntoIterator<Item = S>) -> HostWrite {
    host_write_refs(repo, branches.into_iter().map(|branch| format!("refs/heads/{}", branch.as_ref())))
}

/// [`host_write`] for full ref names, and prefixes ending in `/` for every ref under them, such as
/// `refs/remotes/` for a fetch.
pub fn host_write_refs(repo: &Path, refs: impl IntoIterator<Item = String>) -> HostWrite {
    let names: Vec<String> = refs.into_iter().collect();
    let mut host = HOST_WRITES.lock().unwrap_or_else(|e| e.into_inner());
    host.pending.extend(names.iter().map(|name| (repo.to_path_buf(), name.clone())));
    HostWrite { repo: repo.to_path_buf(), names }
}

/// A write by IMACS in progress; see [`host_write`]. Dropping it runs git.
pub struct HostWrite {
    repo: PathBuf,
    names: Vec<String>,
}

impl Drop for HostWrite {
    fn drop(&mut self) {
        let values = read_refs(&self.repo).ok();
        self.record(values.as_ref());
        if let Some(values) = &values {
            accept_written(&self.repo, &self.names, values);
        }
    }
}

impl HostWrite {
    fn record(&self, values: Option<&BTreeMap<String, RefValue>>) {
        let mut host = HOST_WRITES.lock().unwrap_or_else(|e| e.into_inner());
        for name in &self.names {
            if let Some(index) = host.pending.iter().position(|(repo, pending)| *repo == self.repo && pending == name) {
                host.pending.remove(index);
            }
            // Without the values a later check falls back to restoring, as before IMACS wrote.
            if let Some(values) = values {
                host.written.retain(|(repo, written, _)| !(*repo == self.repo && written == name));
                let left = values.iter().filter(|(other, _)| covers(name, other));
                let left = left.map(|(other, value)| (other.clone(), value.clone())).collect();
                host.written.push((self.repo.clone(), name.clone(), left));
            }
        }
    }
}

/// What IMACS's own writes left in one repository, oldest first; see [`host_write`].
#[derive(Default)]
pub(crate) struct Written(Vec<(String, BTreeMap<String, RefValue>)>);

impl Written {
    /// Whether IMACS's latest write covering `name` left it at `value`.
    fn left(&self, name: &str, value: Option<&RefValue>) -> bool {
        self.0.iter().rev().find(|(pattern, _)| covers(pattern, name)).is_some_and(|(_, refs)| refs.get(name) == value)
    }
}

/// Every open guard, so that what IMACS writes reaches its baseline as soon as it is written.
static OPEN: Mutex<Vec<Weak<Mutex<RefGuard>>>> = Mutex::new(Vec::new());

/// Puts what IMACS just wrote into the baseline of every guard of that repository: the open ones, and
/// the snapshots of sandboxed sessions that are not running, which they reattach with. An agent that
/// moves such a ref afterwards is restored to IMACS's value, not to an older one.
fn accept_written(repo: &Path, names: &[String], values: &BTreeMap<String, RefValue>) {
    let Ok(common) = common_of(repo) else { return };
    let open: Vec<_> = OPEN.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(Weak::upgrade).collect();
    let mut files = HashSet::new();
    for guard in open {
        let mut guard = guard.lock().unwrap_or_else(|e| e.into_inner());
        files.insert(guard.file.clone());
        if guard.common == common {
            let _ = guard.accept(names, values);
        }
    }
    let Ok(entries) = std::fs::read_dir(paths::imacs_home().join("ref-guards")) else { return };
    let snapshots = entries.flatten().map(|entry| entry.path());
    for file in snapshots.filter(|file| file.extension().is_some_and(|ext| ext == "json") && !files.contains(file)) {
        let Some(mut guard) =
            std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<RefGuard>(&bytes).ok())
        else {
            continue;
        };
        if guard.common == common {
            guard.file = file;
            let _ = guard.accept(names, values);
        }
    }
}

fn common_of(cwd: &Path) -> Result<PathBuf> {
    let common = git::git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    Ok(PathBuf::from(common.trim()).canonicalize()?)
}

/// The refs IMACS is writing, as names and prefixes, and the values it left, in the repository at
/// `common`. Runs git.
fn host_writes(common: &Path) -> (BTreeSet<String>, Written) {
    let (pending, written) = {
        let host = HOST_WRITES.lock().unwrap_or_else(|e| e.into_inner());
        (host.pending.clone(), host.written.clone())
    };
    let mut commons: HashMap<PathBuf, bool> = HashMap::new();
    let mut ours = |repo: &PathBuf| {
        *commons.entry(repo.clone()).or_insert_with(|| git::common_dir(repo).ok().as_deref() == Some(common))
    };
    let pending = pending.into_iter().filter(|(repo, _)| ours(repo)).map(|(_, name)| name).collect();
    let written = written.into_iter().filter(|(repo, _, _)| ours(repo)).map(|(_, name, refs)| (name, refs));
    (pending, Written(written.collect()))
}

/// What git on this machine left since a check, read from files no sandbox writes (see the module
/// docs). Reading them runs no git and never waits.
#[derive(Default)]
struct HostLeft {
    /// Each branch a checkout here moved, with the commit it last left it at.
    branches: BTreeSet<(String, String)>,
    /// Each branch a checkout here switched to, at its commit then: one a switch made (`git switch -c`).
    switched: BTreeSet<(String, String)>,
    /// Objects fetches here wrote to FETCH_HEAD, and whether any fetch ran: it may have pruned refs.
    fetched: BTreeSet<String>,
    fetch: bool,
}

/// Bounds what a check reads of a HEAD log or FETCH_HEAD; the newest entries are at the end.
const TRACE_LIMIT: u64 = 1 << 20;

impl HostLeft {
    /// The checkouts on this machine in the repository at `common`: the main checkout, and each linked
    /// worktree but `own`, the guarded one, and those of other sandboxes, whose git folders they write.
    fn read(common: &Path, own: Option<&Path>, since: u64) -> Self {
        let sandboxed: Vec<PathBuf> = git::pinned_all()
            .into_iter()
            .filter(|pins| pins.git_dir != pins.common_dir)
            .filter_map(|pins| pins.git_dir)
            .collect();
        let mut host = Self::default();
        host.read_checkout(common, &common.join("logs/HEAD"), since, false);
        let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else { return host };
        for entry in entries.flatten() {
            let folder = entry.path();
            let real = folder.canonicalize().unwrap_or_else(|_| folder.clone());
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && own != Some(&real) && !sandboxed.contains(&real) {
                host.read_checkout(&folder, &folder.join("logs/HEAD"), since, true);
            }
        }
        host
    }

    /// One checkout's traces since `since`: the git `folder` it writes and its HEAD `log`.
    fn read_checkout(&mut self, folder: &Path, log: &Path, since: u64, private_log: bool) {
        if let Some((bytes, modified)) = read_trace(&folder.join("FETCH_HEAD"))
            && modified >= since
            && modified <= now()
        {
            self.fetch = true;
            let text = String::from_utf8_lossy(&bytes);
            let objects = text.lines().filter_map(|line| line.split('\t').next()).filter(|id| is_object(id));
            self.fetched.extend(objects.map(str::to_string));
        }
        let Some((bytes, _)) = read_trace(log) else { return };
        let text = String::from_utf8_lossy(&bytes);
        let until = now();
        let entries: Vec<_> = text
            .lines()
            .filter_map(LogEntry::parse)
            .filter(|entry| entry.time >= since && entry.time <= until)
            .collect();
        // Newest first, from the branch HEAD is on now: before a switch, HEAD was on the branch it left.
        let mut branch = read_trace(&folder.join("HEAD"))
            .and_then(|(bytes, _)| String::from_utf8(bytes).ok()?.trim().strip_prefix("ref: ").map(str::to_string));
        let mut moved = BTreeMap::new();
        for entry in entries.iter().rev() {
            match entry.message.strip_prefix("checkout: moving from ").and_then(|rest| rest.split_once(" to ")) {
                Some((from, _)) => {
                    self.switched.extend(branch.take().map(|to| (to, entry.new.clone())));
                    // The main HEAD is host-only, but its log is shared with sandboxes.
                    // A checkout message there cannot vouch for a different branch.
                    if !private_log {
                        break;
                    }
                    branch = Some(format!("refs/heads/{from}"));
                }
                // An entry that leaves HEAD where it was, such as a stash's reset, vouches for nothing:
                // its value is whatever the branch held then, an agent's too.
                None if entry.old != entry.new => {
                    if let Some(branch) = &branch {
                        moved.entry(branch.clone()).or_insert_with(|| entry.new.clone());
                    }
                }
                None => {}
            }
        }
        self.branches.extend(moved);
    }

    /// Whether git on this machine left `name`, which was at `before`, at `after`.
    fn left(&self, name: &str, before: Option<&RefValue>, after: Option<&RefValue>) -> bool {
        let fetched = name.starts_with("refs/remotes/") || name.starts_with("refs/tags/");
        match after {
            Some(RefValue::Object(object)) => {
                let pair = (name.to_string(), object.clone());
                self.branches.contains(&pair)
                    || (before.is_none() && self.switched.contains(&pair))
                    || (fetched && self.fetched.contains(object))
            }
            Some(RefValue::Symbolic(target)) => fetched && self.fetch && target.starts_with("refs/remotes/"),
            None => fetched && self.fetch,
        }
    }
}

/// One line of a reflog: `<old> <new> <name> <<email>> <time> <zone>\t<message>`.
struct LogEntry {
    old: String,
    new: String,
    time: u64,
    message: String,
}

impl LogEntry {
    fn parse(line: &str) -> Option<Self> {
        let (fields, message) = line.split_once('\t').unwrap_or((line, ""));
        let mut forward = fields.splitn(3, ' ');
        let (old, new) = (forward.next()?, forward.next()?);
        let mut back = fields.rsplitn(3, ' ');
        let (_zone, time) = (back.next()?, back.next()?.parse().ok()?);
        (is_object(old) && is_object(new)).then(|| Self {
            old: old.into(),
            new: new.into(),
            time,
            message: message.into(),
        })
    }
}

fn is_object(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The end of a regular file, at most [`TRACE_LIMIT`] bytes, and when it last changed in seconds.
/// Never follows a link at the file or waits on a FIFO.
fn read_trace(path: &Path) -> Option<(Vec<u8>, u64)> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file =
        std::fs::File::options().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    file.seek(SeekFrom::Start(metadata.len().saturating_sub(TRACE_LIMIT))).ok()?;
    let mut bytes = Vec::new();
    file.take(TRACE_LIMIT).read_to_end(&mut bytes).ok()?;
    let modified = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((bytes, modified))
}

#[derive(Serialize, Deserialize)]
pub struct RefGuard {
    pub common: PathBuf,
    pub branch: Option<String>,
    refs: BTreeMap<String, RefValue>,
    /// When the baseline was last taken, in seconds: host traces from then on count.
    #[serde(default)]
    checked: Option<u64>,
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
        let common = common_of(cwd)?;
        let mut guard = if file.exists() {
            let mut guard: Self = serde_json::from_slice(&std::fs::read(&file)?)?;
            if guard.common != common {
                bail!("ref guard snapshot belongs to another repository");
            }
            // A snapshot from before IMACS kept the time was saved at its last check.
            if guard.checked.is_none() {
                let modified = std::fs::metadata(&file)?.modified()?;
                guard.checked = Some(modified.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs());
            }
            guard
        } else {
            Self {
                common,
                branch: git::git(cwd, &["symbolic-ref", "-q", "HEAD"]).ok().map(|s| s.trim().to_string()),
                checked: Some(now()),
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

    /// Restore changed, deleted and newly created refs that neither the task branches nor this
    /// machine account for (see the module docs). Other active tasks' branches must be exempt too:
    /// they share this same refs directory. `concurrent` holds those and the refs IMACS is writing, as
    /// names or prefixes; a ref still at the value IMACS `written` there is accepted.
    pub(crate) fn check(&mut self, concurrent: &BTreeSet<String>, written: &Written) -> Result<Vec<String>> {
        let started = now();
        let current = read_refs(&self.cwd)?;
        let own = git::git(&self.cwd, &["rev-parse", "--absolute-git-dir"])
            .ok()
            .and_then(|dir| PathBuf::from(dir.trim()).canonicalize().ok());
        // A second's slack: reflogs and file times are in whole seconds.
        let host = HostLeft::read(&self.common, own.as_deref(), self.checked.unwrap_or(started).saturating_sub(1));
        let session = self.file.file_stem().map_or_else(|| "unknown".into(), |stem| stem.to_string_lossy());
        let names: BTreeSet<_> = self.refs.keys().chain(current.keys()).cloned().collect();
        // A push updates a tracking ref without FETCH_HEAD. Only already accepted local
        // tips vouch for that move; a newly injected branch must not launder it.
        let local_tips: BTreeSet<_> = current
            .iter()
            .filter_map(|(name, value)| {
                (name.starts_with("refs/heads/")
                    && (self.refs.get(name) == Some(value)
                        || self.branch.as_ref() == Some(name)
                        || concurrent.iter().any(|pattern| covers(pattern, name))
                        || written.left(name, Some(value))
                        || host.left(name, self.refs.get(name), Some(value))))
                .then_some(value)
                .and_then(|value| match value {
                    RefValue::Object(object) => Some(object.clone()),
                    _ => None,
                })
            })
            .collect();
        let mut alerts = Vec::new();
        for name in names {
            let (before, after) = (self.refs.get(&name), current.get(&name));
            let accepted = self.branch.as_ref() == Some(&name)
                || concurrent.iter().any(|pattern| covers(pattern, &name))
                || UNGUARDED.iter().any(|pattern| covers(pattern, &name))
                || written.left(&name, after)
                || host.left(&name, before, after)
                || name.starts_with("refs/remotes/")
                    && matches!(after, Some(RefValue::Object(object)) if local_tips.contains(object));
            if accepted {
                match after {
                    Some(value) => {
                        self.refs.insert(name, value.clone());
                    }
                    None => {
                        self.refs.remove(&name);
                    }
                }
                continue;
            }
            if before == after {
                continue;
            }
            let (expected, found) = (shown(before), shown(after));
            // The value being replaced stays reachable, whoever wrote it.
            let kept = match after {
                Some(RefValue::Object(_)) => Some(format!("{QUARANTINE}{session}/{name}@{found}")),
                _ => None,
            };
            let result = restore(&self.cwd, &name, before, after, kept.as_deref());
            alerts.push(match (result, before, kept) {
                (Ok(()), None, Some(kept)) => format!("Restored {name}: moved it to {kept} (it pointed to {found})"),
                (Ok(()), None, None) => format!("Restored {name}: removed it (it was {found})"),
                (Ok(()), _, _) if after.is_none() => format!("Restored {name}: recreated it at {expected}"),
                (Ok(()), _, Some(kept)) => {
                    format!("Restored {name}: from {found} back to {expected}; kept {found} at {kept}")
                }
                (Ok(()), _, None) => format!("Restored {name}: from {found} back to {expected}"),
                (Err(error), _, _) => {
                    format!("RESTORE FAILED for {name}: {error:#}; expected {expected}, found {found}")
                }
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
        self.checked = Some(started);
        self.save()?;
        Ok(alerts)
    }

    /// Takes the values IMACS left under `names`, ref names or prefixes, as this guard's baseline for them.
    fn accept(&mut self, names: &[String], values: &BTreeMap<String, RefValue>) -> Result<()> {
        let ours = |name: &String| names.iter().any(|pattern| covers(pattern, name));
        self.refs.retain(|name, _| !ours(name));
        self.refs
            .extend(values.iter().filter(|(name, _)| ours(name)).map(|(name, value)| (name.clone(), value.clone())));
        self.save()
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
        let (common, branch) = (guard.common.clone(), guard.branch.clone());
        let guard = Arc::new(Mutex::new(guard));
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|open| open.strong_count() > 0);
        open.push(Arc::downgrade(&guard));
        Self { common, branch, guard }
    }

    /// Runs git: call it on [`Checks`]' thread.
    pub fn check(&self, exempt: &Exempt) -> Result<Vec<String>> {
        let mut concurrent = exempt.resolve(&self.common);
        let (pending, written) = host_writes(&self.common);
        concurrent.extend(pending);
        self.guard.lock().unwrap_or_else(|e| e.into_inner()).check(&concurrent, &written)
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

    /// Wait only from shutdown or a cleanup worker, never the interactive runtime loop.
    pub fn finish(&self) {
        let (done, finished) = channel();
        self.run(move || {
            let _ = done.send(());
        });
        let _ = finished.recv();
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
            // Checked by type above, but the sandbox can swap in a FIFO since.
            let value = git::read_regular_file(&entry.path(), 64 * 1024)?;
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

/// Compare-and-swap under Git's ref lock, keeping the object `after` at the ref `kept`. `no-deref`
/// prevents a malicious symbolic ref from making restoration write through to a different branch.
fn restore(
    cwd: &Path,
    name: &str,
    before: Option<&RefValue>,
    after: Option<&RefValue>,
    kept: Option<&str>,
) -> Result<()> {
    let keep = match (after, kept) {
        (Some(RefValue::Object(object)), Some(kept)) => format!("update {kept} {object}\n"),
        _ => String::new(),
    };
    // Older Git versions have no symbolic-ref transactions. Recheck before those operations;
    // ordinary branch/tag OIDs below use Git's atomic old-value check.
    if matches!(before, Some(RefValue::Symbolic(_))) || matches!(after, Some(RefValue::Symbolic(_))) {
        if read_refs(cwd)?.get(name) != after {
            bail!("ref changed again during recovery");
        }
        if !keep.is_empty() {
            update_refs(cwd, &keep)?;
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
    // One transaction: the value is kept aside only if the ref is put back.
    update_refs(cwd, &format!("{keep}{operation}\n"))
}

/// Runs `operations` as one `git update-ref --stdin` transaction without dereferencing.
fn update_refs(cwd: &Path, operations: &str) -> Result<()> {
    let input = format!("start\noption no-deref\n{operations}prepare\ncommit\n");
    git::git_input(cwd, &["update-ref", "--stdin"], input.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_push_keeps_tracking_refs_only_at_accepted_local_tips() {
        let (dir, repo, worktree) = repository();
        let remote = dir.path().join("remote.git");
        git::git(&repo, &["init", "--bare", remote.to_str().unwrap()]).unwrap();
        git::git(&repo, &["remote", "add", "origin", remote.to_str().unwrap()]).unwrap();
        git::git(&repo, &["push", "origin", "main"]).unwrap();
        let mut guard = RefGuard::open(&worktree, dir.path().join("snapshot.json")).unwrap().unwrap();
        git::git(&repo, &["commit", "--allow-empty", "-qm", "user work"]).unwrap();
        let accepted = git::git(&repo, &["rev-parse", "HEAD"]).unwrap();
        git::git(&repo, &["push", "origin", "main"]).unwrap();
        // An unaccepted local branch cannot launder a tracking-ref move.
        let injected = git::git(&worktree, &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "agent work"]).unwrap();
        git::git(&worktree, &["update-ref", "refs/heads/forged", injected.trim()]).unwrap();
        git::git(&worktree, &["update-ref", "refs/remotes/origin/forged", injected.trim()]).unwrap();
        let alerts = guard.check(&Default::default(), &Default::default()).unwrap();
        assert!(!alerts.iter().any(|a| a.contains("refs/remotes/origin/main")), "{alerts:?}");
        assert!(alerts.iter().any(|a| a.contains("Restored refs/remotes/origin/forged")), "{alerts:?}");
        assert_eq!(git::git(&repo, &["rev-parse", "origin/main"]).unwrap(), accepted);
    }

    #[test]
    fn shared_head_log_cannot_reattribute_other_branches_or_vouch_for_future_moves() {
        for future in [false, true] {
            let (dir, repo, worktree) = repository();
            git::git(&repo, &["branch", "victim"]).unwrap();
            let mut guard = RefGuard::open(&worktree, dir.path().join("snapshot.json")).unwrap().unwrap();
            let initial = git::git(&repo, &["rev-parse", "main"]).unwrap().trim().to_string();
            git::git(&worktree, &["commit", "--allow-empty", "-qm", "injected"]).unwrap();
            let injected = git::git(&worktree, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
            let branch = if future { "main" } else { "victim" };
            git::git(&worktree, &["update-ref", &format!("refs/heads/{branch}"), &injected]).unwrap();
            let time = now() + if future { 3600 } else { 0 };
            let mut forged = format!("{initial} {injected} Agent <agent@example.org> {time} +0000\tcommit: forged\n");
            if !future {
                forged.push_str(&format!("{injected} {initial} Agent <agent@example.org> {time} +0000\tcheckout: moving from victim to main\n"));
            }
            std::fs::write(repo.join(".git/logs/HEAD"), forged).unwrap();
            let alerts = guard.check(&Default::default(), &Default::default()).unwrap();
            assert!(alerts.iter().any(|a| a.contains(&format!("Restored refs/heads/{branch}"))), "{alerts:?}");
            assert_eq!(git::git(&repo, &["rev-parse", branch]).unwrap().trim(), initial);
        }
    }

    fn repository() -> (crate::test_support::TempDir, PathBuf, PathBuf) {
        let dir = crate::test_support::tempdir().unwrap();
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
        let alerts = guard.check(&BTreeSet::new(), &Written::default()).unwrap();
        assert_eq!(alerts.len(), 6, "{alerts:?}");
        assert!(alerts.iter().all(|alert| alert.starts_with("Restored")), "{alerts:?}");
        for name in ["main", "other", "original", "origin/HEAD"] {
            assert_eq!(git::git(&repo, &["rev-parse", name]).unwrap().trim(), initial, "{name}");
        }
        assert_eq!(git::git(&repo, &["rev-parse", "task"]).unwrap().trim(), task);
        assert!(git::git(&repo, &["rev-parse", "--verify", "refs/tags/injected"]).is_err());
        assert!(git::git(&repo, &["symbolic-ref", "refs/heads/dangling"]).is_err());
        // Nothing is deleted: what the agent wrote is kept aside.
        for name in ["refs/heads/main", "refs/tags/injected"] {
            let kept = format!("refs/imacs/quarantine/snapshot/{name}@{}", &task[..12]);
            assert_eq!(git::git(&repo, &["rev-parse", &kept]).unwrap().trim(), task, "{alerts:?}");
        }
        assert!(guard.check(&BTreeSet::new(), &Written::default()).unwrap().is_empty());
    }

    #[test]
    fn this_machines_commits_fetches_and_stashes_survive_a_turn_and_agent_refs_are_quarantined() {
        let (dir, repo, worktree) = repository();
        let origin = dir.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        for args in [
            &["init", "-qb", "main"][..],
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.org",
                "commit",
                "--allow-empty",
                "-qm",
                "upstream",
            ],
            &["tag", "v1"],
        ] {
            git::git(&origin, args).unwrap();
        }
        git::git(&repo, &["remote", "add", "origin", &origin.display().to_string()]).unwrap();
        git::git(&repo, &["branch", "other"]).unwrap();
        let initial = git::git(&repo, &["rev-parse", "main"]).unwrap().trim().to_string();
        let guard = RefGuard::open(&worktree, dir.path().join("imacs-agent.json")).unwrap().unwrap();
        let guard = Shared::new(guard);

        // During the turn the agent commits, moves a branch and makes one of its own...
        git::git(&worktree, &["commit", "--allow-empty", "-qm", "agent"]).unwrap();
        let agent = git::git(&worktree, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        git::git(&worktree, &["update-ref", "refs/heads/other", &agent]).unwrap();
        git::git(&worktree, &["branch", "agent-made"]).unwrap();
        // ...while you commit on main, switch away, fetch and stash in your checkout.
        git::git(&repo, &["commit", "--allow-empty", "-qm", "mine"]).unwrap();
        let mine = git::git(&repo, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        git::add_worktree(&repo, &dir.path().join("host"), "later", "main").unwrap();
        git::git(&repo, &["fetch", "-q", "origin"]).unwrap();
        std::fs::write(repo.join("scratch"), "work in progress").unwrap();
        git::git(&repo, &["add", "scratch"]).unwrap();
        git::git(&repo, &["stash", "-q"]).unwrap();

        let alerts = guard.check(&Exempt::default()).unwrap();
        assert_eq!(alerts.len(), 2, "{alerts:?}");
        assert_eq!(git::git(&repo, &["rev-parse", "main"]).unwrap().trim(), mine);
        for name in ["later", "origin/main", "v1", "stash@{0}"] {
            assert!(git::git(&repo, &["rev-parse", "--verify", name]).is_ok(), "{name}: {alerts:?}");
        }
        assert_eq!(git::git(&repo, &["rev-parse", "other"]).unwrap().trim(), initial);
        assert!(git::git(&repo, &["rev-parse", "--verify", "refs/heads/agent-made"]).is_err());
        for name in ["other", "agent-made"] {
            let kept = format!("refs/imacs/quarantine/imacs-agent/refs/heads/{name}@{}", &agent[..12]);
            assert_eq!(git::git(&repo, &["rev-parse", &kept]).unwrap().trim(), agent, "{alerts:?}");
            assert!(alerts.iter().any(|alert| alert.contains(&kept)), "{alerts:?}");
        }

        // A fetch that leaves no FETCH_HEAD is IMACS's own only while IMACS says so.
        git::git(
            &origin,
            &["-c", "user.name=Test", "-c", "user.email=test@example.org", "commit", "--allow-empty", "-qm", "next"],
        )
        .unwrap();
        let next = git::git(&origin, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        let write = host_write_refs(&repo, ["refs/remotes/".into(), "refs/tags/".into()]);
        git::git(&repo, &["fetch", "-q", "--no-write-fetch-head", "origin"]).unwrap();
        drop(write);
        assert!(guard.check(&Exempt::default()).unwrap().is_empty());
        assert_eq!(git::git(&repo, &["rev-parse", "origin/main"]).unwrap().trim(), next);
        git::git(
            &origin,
            &["-c", "user.name=Test", "-c", "user.email=test@example.org", "commit", "--allow-empty", "-qm", "last"],
        )
        .unwrap();
        git::git(
            &worktree,
            &["fetch", "-q", "--no-write-fetch-head", &origin.display().to_string(), "+main:refs/remotes/origin/main"],
        )
        .unwrap();
        let alerts = guard.check(&Exempt::default()).unwrap();
        assert!(alerts.len() == 1 && alerts[0].contains("Restored refs/remotes/origin/main"), "{alerts:?}");
        assert_eq!(git::git(&repo, &["rev-parse", "origin/main"]).unwrap().trim(), next);

        // A stash after the agent moved your checked-out branch logs HEAD at the agent's commit,
        // without moving it: that vouches for nothing.
        git::git(&worktree, &["update-ref", "refs/heads/later", &agent]).unwrap();
        std::fs::write(repo.join("scratch"), "more work").unwrap();
        git::git(&repo, &["add", "scratch"]).unwrap();
        git::git(&repo, &["stash", "-q"]).unwrap();
        let alerts = guard.check(&Exempt::default()).unwrap();
        assert!(alerts.len() == 1 && alerts[0].contains("Restored refs/heads/later"), "{alerts:?}");
        assert_eq!(git::git(&repo, &["rev-parse", "later"]).unwrap().trim(), mine);
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
        assert!(guard.check(&BTreeSet::from(["refs/heads/peer".into()]), &Written::default()).unwrap().is_empty());
        assert!(
            guard.check(&BTreeSet::new(), &Written::default()).unwrap().is_empty(),
            "accepted commit survives peer exit"
        );
        // From the sandbox's worktree: the main checkout's own update-ref would leave its HEAD log a trace.
        git::git(&worktree, &["update-ref", "refs/heads/main", task.trim()]).unwrap();
        let lock = repo.join(".git/refs/heads/main.lock");
        std::fs::write(&lock, "").unwrap();
        let alerts = guard.check(&BTreeSet::new(), &Written::default()).unwrap();
        assert!(alerts[0].contains("RESTORE FAILED"), "{alerts:?}");
        std::fs::remove_file(lock).unwrap();
        assert!(guard.check(&BTreeSet::new(), &Written::default()).unwrap()[0].starts_with("Restored"));
        assert_eq!(git::git(&repo, &["rev-parse", "main"]).unwrap(), initial);
    }
}
