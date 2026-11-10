//! Cleanup owned resources, with failures kept visible in doctor and the cleanup dialog.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use atomics_core::{cleanup, config::Config, git, plan::Plan, store::Store};

use crate::sandbox;
use crate::sandbox_ownership::{self as ownership, Resource};

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Background work that stops or removes sandboxes and worktrees: a closed session's stop and final
/// ref check, a plan's cleanup, the cleanup dialog's removals. Work that must not overlap it, such as
/// preparing the same sandbox or making a worktree where one is being removed, waits for it on a
/// thread of its own; the UI thread never does.
#[derive(Clone)]
pub struct Job {
    done: Arc<(Mutex<bool>, Condvar)>,
    scope: Scope,
}

/// What a [`Job`] may stop or remove. `None` is anything, for work that finds its targets as it runs.
#[derive(Clone, Default)]
pub struct Scope {
    pub sandboxes: Vec<String>,
    pub worktrees: Option<Vec<PathBuf>>,
}

impl Scope {
    pub fn sandbox(name: String) -> Self {
        Self { sandboxes: vec![name], worktrees: Some(Vec::new()) }
    }

    /// Work that stops or removes nothing, such as a scan.
    pub fn none() -> Self {
        Self { sandboxes: Vec::new(), worktrees: Some(Vec::new()) }
    }
}

/// Marks its job done when its thread ends, also by a panic.
struct Done(Arc<(Mutex<bool>, Condvar)>);

impl Drop for Done {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("ATOMICS: cleanup worker panicked");
        }
        let (done, changed) = &*self.0;
        *done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        changed.notify_all();
    }
}

impl Job {
    pub fn spawn(scope: Scope, work: impl FnOnce() + Send + 'static) -> Self {
        let job = Self { done: Arc::default(), scope };
        let done = Done(job.done.clone());
        std::thread::spawn(move || {
            let _done = done;
            work();
        });
        job
    }

    pub fn is_finished(&self) -> bool {
        *self.done.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether it may stop or remove sandbox `name`, or the worktree at `path`. Compares folders by
    /// where they lead: call it off the UI thread.
    pub fn touches(&self, name: Option<&str>, path: Option<&Path>) -> bool {
        let worktree = |path: &Path| match &self.scope.worktrees {
            None => true,
            Some(worktrees) => worktrees.iter().any(|tree| tree == path || real(tree) == real(path)),
        };
        name.is_some_and(|name| self.scope.sandboxes.iter().any(|other| other == name)) || path.is_some_and(worktree)
    }

    pub fn wait(&self) {
        let (done, changed) = &*self.done;
        let mut finished = done.lock().unwrap_or_else(|e| e.into_inner());
        while !*finished {
            finished = changed.wait(finished).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits at most until `deadline`; whether the job finished.
    pub fn wait_until(&self, deadline: Instant) -> bool {
        let (done, changed) = &*self.done;
        let mut finished = done.lock().unwrap_or_else(|e| e.into_inner());
        while !*finished {
            let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()) else {
                return false;
            };
            finished = changed.wait_timeout(finished, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }
}

/// Waits, off the UI thread, for those of `jobs` that may stop or remove sandbox `name` or the
/// worktree at `path`.
pub fn wait_for(jobs: &[Job], name: Option<&str>, path: Option<&Path>) {
    for job in jobs.iter().filter(|job| job.touches(name, path)) {
        job.wait();
    }
}

/// How a plan cleanup's note starts when ATOMICS's git records could not be loaded, its only reason
/// to skip the plan.
const PLAN_SKIPPED: &str = "Plan cleanup skipped: ";

/// What a plan's cleanup did: notes of what it kept and why, the worktrees it left for now because
/// processes still use them, and the worktrees it removed.
#[derive(Debug, Default)]
pub struct Cleaned {
    pub notes: Vec<String>,
    pub deferred: Vec<Deferred>,
    pub removed: Vec<PathBuf>,
}

/// A closed plan's task worktree whose removal waits: processes still run in it, or hold files open
/// under it, such as a dev server an agent left running for the user. It is removed, as the plan's
/// cleanup would have, once nothing uses it ([`clean_worktrees`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Deferred {
    pub plan: String,
    pub branch: String,
    pub worktree: PathBuf,
    /// What used it when last checked, as `<pid> <name>`.
    pub users: Vec<String>,
    /// When the cleanup first left it, in seconds since the epoch.
    pub since: u64,
}

/// Where the removals waiting for their processes are kept, so a restarted host still makes them.
const DEFERRED: &str = "cleanup-deferred.json";

pub fn deferred(home: &Path) -> Vec<Deferred> {
    std::fs::read(home.join(DEFERRED)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub fn save_deferred(home: &Path, deferred: &[Deferred]) -> Result<()> {
    let path = home.join(DEFERRED);
    if deferred.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(home)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(deferred)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// The processes of this user that use `dir`: their working directory is in it, or they hold a file
/// open under it. Each as `<pid> <name>`; never this process. Empty when none do, or when this
/// machine cannot tell (no `/proc`, no `lsof`).
pub fn processes_in(dir: &Path) -> Vec<String> {
    let dir = real(dir);
    let inside = |path: &Path| path.starts_with(&dir);
    let me = std::process::id();
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
            if pid == me {
                continue;
            }
            let base = entry.path();
            let uses = std::fs::read_link(base.join("cwd")).is_ok_and(|cwd| inside(&cwd))
                || std::fs::read_dir(base.join("fd"))
                    .is_ok_and(|fds| fds.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|f| inside(&f))));
            if uses {
                let name = std::fs::read_to_string(base.join("comm")).unwrap_or_default();
                found.push(format!("{pid} {}", name.trim()));
            }
        }
        return found;
    }
    // macOS: lsof lists every process's working directory and open files, as `p<pid>`, `c<name>` and
    // `n<path>` lines.
    let Ok(output) = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-w", "-F", "pcn"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return found;
    };
    let (mut pid, mut name, mut counted) = (0u32, String::new(), false);
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        match line.split_at_checked(1) {
            Some(("p", value)) => (pid, counted) = (value.parse().unwrap_or(0), false),
            Some(("c", value)) => name = value.to_string(),
            Some(("n", value)) if !counted && pid != 0 && pid != me && inside(Path::new(value)) => {
                counted = true;
                found.push(format!("{pid} {name}"));
            }
            _ => {}
        }
    }
    found
}

pub fn clean_plan(home: &Path, plan: &Plan, sbx: &str, protected: &[PathBuf]) -> Cleaned {
    clean_plan_in(home, plan, sbx, protected, None)
}

/// [`clean_plan`] for those of the plan's worktrees in `worktrees` only: the ones it left for their
/// processes, checked again, or one whose session has just closed.
pub fn clean_worktrees(home: &Path, plan: &Plan, sbx: &str, protected: &[PathBuf], worktrees: &[PathBuf]) -> Cleaned {
    clean_plan_in(home, plan, sbx, protected, Some(worktrees))
}

/// What became of one of a plan's task branches.
enum Step {
    Removed { branch: bool },
    InUse(Vec<String>),
}

fn clean_plan_in(home: &Path, plan: &Plan, sbx: &str, protected: &[PathBuf], only: Option<&[PathBuf]>) -> Cleaned {
    let mut cleaned = Cleaned::default();
    if let Err(error) = sandbox::load_owned_pins(home) {
        cleaned.notes.push(format!("{PLAN_SKIPPED}{error:#}"));
        return cleaned;
    }
    let entries = match cleanup::closed_tasks(plan) {
        Ok(entries) => entries,
        Err(err) => {
            cleaned.notes.push(format!("Plan cleanup: {err:#}"));
            return cleaned;
        }
    };
    for entry in entries {
        if let Some(only) = only
            && !entry.worktree.as_ref().is_some_and(|path| only.iter().any(|o| real(o) == real(path)))
        {
            continue;
        }
        let result = (|| -> Result<Step> {
            let mut names = Vec::new();
            // Kept worktrees are never entered: not to check them, nor to stop their sandboxes.
            if let Some(keep) = &entry.keep {
                anyhow::bail!("kept: {keep}");
            }
            if let Some(path) = &entry.worktree {
                anyhow::ensure!(
                    !protected.iter().any(|root| real(root) == real(path)),
                    "another session uses {}",
                    path.display()
                );
                // Recheck dirt before even stopping a sandbox; git checks again on removal.
                anyhow::ensure!(
                    git::git(path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty(),
                    "uncommitted changes in {}",
                    path.display()
                );
                // A server an agent left running for the user keeps its folder until it stops.
                let users = processes_in(path);
                if !users.is_empty() {
                    return Ok(Step::InUse(users));
                }
                if sandbox::installed(sbx) {
                    names = sandbox::in_worktree(home, sbx, path)?;
                    for name in &names {
                        sandbox::stop(home, sbx, name)?;
                    }
                }
            }
            let write = crate::ref_guard::host_write(&entry.repo, [&entry.branch]);
            let removed_branch = cleanup::remove_closed_task(plan, &entry);
            drop(write);
            // A branch deletion can fail after its worktree was removed. Its sandbox must
            // still go away; otherwise its old mount could be reused by a new checkout.
            if entry.worktree.as_ref().is_some_and(|path| !path.exists()) {
                for name in names {
                    sandbox::remove(home, sbx, &name)?;
                }
            }
            removed_branch.map(|branch| Step::Removed { branch })
        })();
        let worktree = entry.worktree.clone();
        match result {
            Ok(Step::InUse(users)) => {
                let path = worktree.expect("only a worktree is in use");
                cleaned.notes.push(format!(
                    "Kept worktree {} for now: {} still {} in it ({}); it is removed once nothing does",
                    path.display(),
                    if users.len() == 1 { "1 process".to_string() } else { format!("{} processes", users.len()) },
                    if users.len() == 1 { "runs" } else { "run" },
                    shown_users(&users)
                ));
                cleaned.deferred.push(Deferred {
                    plan: plan.id.clone(),
                    branch: entry.branch.clone(),
                    worktree: path,
                    users,
                    since: atomics_core::store::now(),
                });
            }
            Ok(Step::Removed { branch }) => {
                cleaned.removed.extend(worktree.filter(|path| !path.exists()));
                if !branch && plan.status == atomics_core::plan::PlanStatus::Completed {
                    cleaned.notes.push(format!(
                        "Kept branch {}: not merged into {}",
                        entry.branch,
                        plan.integration_branch()
                    ));
                }
            }
            Err(err) => cleaned.notes.push(format!("Cleanup {}: {err:#}", entry.branch)),
        }
    }
    cleaned
}

/// Processes as a note names them: the first few, and how many more.
pub fn shown_users(users: &[String]) -> String {
    let mut shown = users.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
    if users.len() > 4 {
        shown.push_str(&format!(" and {} more", users.len() - 4));
    }
    shown
}

/// Failures kept until someone looks: a session's or plan's cleanup, a ref guard's alert.
const KEPT_LEFTOVERS: &str = "cleanup-leftovers.json";
/// The last startup sweep's failures. Each sweep looks at all of it again and replaces them.
const SWEEP_LEFTOVERS: &str = "cleanup-sweep.json";

/// How notes of the startup sweep start, which earlier versions kept with the others.
const SWEEP_NOTES: &[&str] = &[
    "Startup cleanup skipped: ",
    "Sandbox/MCP ",
    "Sandbox inventory: ",
    "MCP inventory: ",
    "Brief ",
    "Worktrees ",
    "Warm worktree ",
    "Warm cache ",
    "Scratch folders: ",
    "Scratch folder: ",
    "Scratch session ",
];

/// How the note ends that cleanup work was cut short by quitting; the next startup sweep finds what
/// it left.
pub const UNFINISHED_AT_QUIT: &str = " sandbox or worktree cleanup jobs were still running at quit";

static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_notes(path: &Path) -> Vec<String> {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write_notes(path: &Path, notes: &[String]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("no folder")?)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(notes)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Persisted failures survive quitting before a worker can deliver its toast: those kept until
/// someone looks, then the last startup sweep's.
pub fn leftovers(home: &Path) -> Vec<String> {
    let mut all = kept_leftovers(home);
    for note in sweep_leftovers(home) {
        if !all.contains(&note) {
            all.push(note);
        }
    }
    all
}

pub fn kept_leftovers(home: &Path) -> Vec<String> {
    read_notes(&home.join(KEPT_LEFTOVERS))
}

/// What the last startup sweep could not clean up; the next one tries again.
pub fn sweep_leftovers(home: &Path) -> Vec<String> {
    read_notes(&home.join(SWEEP_LEFTOVERS))
}

pub fn save_leftovers(home: &Path, notes: &[String]) -> Result<()> {
    write_notes(&home.join(KEPT_LEFTOVERS), notes)
}

pub fn record_leftovers(home: &Path, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    let _guard = WRITE.lock().unwrap_or_else(|err| err.into_inner());
    let mut all = kept_leftovers(home);
    for note in notes {
        if !all.contains(note) {
            all.push(note.clone());
        }
    }
    if let Err(err) = save_leftovers(home, &all) {
        eprintln!("ATOMICS: recording cleanup failures: {err:#}");
    }
}

/// Replaces the last startup sweep's failures with this one's `notes`, so a fixed failure goes
/// away. Kept notes this sweep has looked at again, of those `before` it started, go too: the
/// sweep's own from earlier versions, work cut short by quitting, and, once the sweep could load
/// ATOMICS's git records (`pins_loaded`), plan cleanups skipped because it could not.
pub fn record_sweep(home: &Path, notes: &[String], pins_loaded: bool, before: &[String]) {
    let _guard = WRITE.lock().unwrap_or_else(|err| err.into_inner());
    let kept = kept_leftovers(home);
    let stale = |note: &String| {
        before.contains(note)
            && (SWEEP_NOTES.iter().any(|start| note.starts_with(start))
                || note.ends_with(UNFINISHED_AT_QUIT)
                || (pins_loaded && note.starts_with(PLAN_SKIPPED)))
    };
    let result = (|| -> Result<()> {
        if kept.iter().any(stale) {
            save_leftovers(home, &kept.iter().filter(|note| !stale(note)).cloned().collect::<Vec<_>>())?;
        }
        let path = home.join(SWEEP_LEFTOVERS);
        match notes {
            [] => match std::fs::remove_file(&path) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
                _ => Ok(()),
            },
            notes => write_notes(&path, notes),
        }
    })();
    if let Err(err) = result {
        eprintln!("ATOMICS: recording cleanup failures: {err:#}");
    }
}

/// Run only in the instance owning the control socket. The cutoff predates window construction's
/// return, so resources created while this background sweep runs cannot become candidates.
/// A malformed ownership record fails closed, rather than treating its resources as orphans.
#[cfg(test)]
pub fn sweep(home: &Path, config: &Config, sbx: &str) -> Result<Vec<String>> {
    let before = kept_leftovers(home);
    let notes = sweep_before(home, config, sbx, Cutoff::mark(home)?)?;
    record_sweep(home, &notes, true, &before);
    Ok(notes)
}

/// When a sweep starts: nothing created from then on is a candidate. The ownership ledger records
/// clock readings, but file times come from the filesystem's clock, which on Linux trails the clock
/// by up to a tick, so a brief written just after a clock reading can look older than it. Files are
/// compared with a marker the filesystem stamped instead, and one as old as the marker counts as new.
pub struct Cutoff {
    time: std::time::SystemTime,
    files: std::time::SystemTime,
}

impl Cutoff {
    pub fn mark(home: &Path) -> Result<Self> {
        std::fs::create_dir_all(home)?;
        let marker = home.join("sweep-cutoff");
        std::fs::write(&marker, "the last cleanup sweep's cutoff\n").context("marking the cleanup cutoff")?;
        let files = std::fs::metadata(&marker)?.modified()?;
        Ok(Self { time: std::time::SystemTime::now(), files })
    }

    /// The marker's file time, for comparing other file times with.
    pub fn files(&self) -> std::time::SystemTime {
        self.files
    }
}

pub fn sweep_before(home: &Path, config: &Config, sbx: &str, cutoff: Cutoff) -> Result<Vec<String>> {
    let Cutoff { time: before, files } = cutoff;
    sandbox::heal_pin_records(home, sbx)?;
    sandbox::load_owned_pins(home)?;
    let store = match std::fs::read(home.join("sessions.json")) {
        Ok(bytes) => serde_json::from_slice::<Store>(&bytes).context("reading cleanup session ownership")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(err) => return Err(err.into()),
    };
    let (plans, errors) = atomics_core::plan::load_plans(home)?;
    anyhow::ensure!(errors.is_empty(), "unreadable plans: {}", errors.join("; "));
    let records: Vec<_> = store.sessions.iter().chain(store.restart.iter().flat_map(|r| r.sessions.iter())).collect();
    let mut sessions: HashSet<String> =
        records.iter().flat_map(|r| [r.pane.clone(), Some(r.claude_session_id.clone())]).flatten().collect();
    // History records are resumable, but a closed plan's generated brief is no longer
    // needed. A restart handoff can still own it while an agent is being restored.
    for task in plans.iter().filter(|p| !p.is_open()).flat_map(|p| &p.tasks) {
        if let Some(id) = &task.session {
            sessions.remove(id);
        }
        if let Some(id) = &task.resume_id {
            sessions.remove(id);
        }
    }
    sessions.extend(
        store
            .restart
            .iter()
            .flat_map(|r| &r.sessions)
            .flat_map(|r| [r.pane.clone(), Some(r.claude_session_id.clone())].into_iter().flatten()),
    );
    sessions.extend(plans.iter().filter(|p| p.is_open()).flat_map(|p| &p.tasks).filter_map(|t| t.session.clone()));
    let sandboxes: HashSet<_> = records
        .iter()
        .filter_map(|r| r.sandbox.clone())
        .chain(sessions.iter().map(|id| format!("atomics-{id}")))
        .collect();
    let mut notes = Vec::new();
    let owned = ownership::owned(home)?;
    let candidates: Vec<_> = owned
        .iter()
        .filter(|(name, entry)| {
            entry.created < ownership::timestamp(before) && !sandboxes.contains(*name) && !ownership::building(entry)
        })
        .collect();
    if !candidates.is_empty() && sandbox::installed(sbx) {
        match sandbox::list(sbx) {
            Ok(list) => match sandbox::mcp_names(sbx) {
                Ok(names) => {
                    for (name, entry) in candidates {
                        if (entry.sandbox && !list.iter().any(|s| s.name == *name)
                            || entry.mcp && !names.contains(name))
                            && ownership::may_be_creating(entry)
                        {
                            continue;
                        }
                        let result = (|| -> Result<()> {
                            if entry.sandbox && !list.iter().any(|s| s.name == *name) {
                                ownership::removed(home, name, Resource::Sandbox)?;
                            }
                            if entry.mcp && !names.contains(name) {
                                ownership::removed(home, name, Resource::Mcp)?;
                            }
                            sandbox::remove(home, sbx, name)
                        })();
                        if let Err(err) = result {
                            notes.push(format!("Sandbox/MCP {name}: {err:#}"));
                        }
                    }
                }
                Err(err) => notes.push(format!("MCP inventory: {err:#}")),
            },
            Err(err) => notes.push(format!("Sandbox inventory: {err:#}")),
        }
    }
    let briefs = home.join("briefs");
    if briefs.exists() {
        for entry in std::fs::read_dir(&briefs)? {
            let entry = entry?;
            if entry.metadata()?.modified()? >= files {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".md").unwrap_or(&name);
            if sessions.contains(id) {
                continue;
            }
            let result =
                if entry.file_type()?.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
            if let Err(err) = result {
                notes.push(format!("Brief {}: {err}", path.display()));
            }
        }
    }
    let mut repos: Vec<_> = config
        .repos
        .iter()
        .map(|r| atomics_core::paths::expand(r))
        .chain(store.projects)
        .chain(store.recent_projects)
        .chain(plans.iter().filter_map(|p| p.project.clone()))
        .collect();
    repos.sort();
    repos.dedup();
    let protected: HashSet<_> = records
        .iter()
        .map(|r| real(&r.cwd))
        .chain(plans.iter().flat_map(|p| p.tasks.iter()).filter_map(|t| t.worktree.as_deref()).map(real))
        .collect();
    for repo in repos {
        // A project folder deleted since, such as a scratch one, has no worktrees left to sweep.
        if std::fs::symlink_metadata(&repo).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound) {
            continue;
        }
        let result = (|| -> Result<()> {
            // Git never runs in a worktree listed from files that do not lead back to it.
            let trees: Vec<_> = git::list_worktrees(&repo)?.into_iter().filter(|tree| tree.trusted).collect();
            for tree in &trees {
                sweep_warm_stages(&tree.path, &config.worktrees.warm, &mut notes)?;
            }
            for tree in trees.into_iter().skip(1) {
                let warm = tree
                    .branch
                    .as_deref()
                    .is_some_and(|b| b.starts_with("atomics/warm-") || b.starts_with("atomics-warm-"));
                if !warm
                    || protected.contains(&real(&tree.path))
                    || std::fs::metadata(&tree.path).and_then(|m| m.modified()).is_ok_and(|modified| modified >= files)
                {
                    continue;
                }
                let result = (|| -> Result<()> {
                    anyhow::ensure!(
                        git::git(&tree.path, &["status", "--porcelain", "--untracked-files=all"])?.is_empty(),
                        "uncommitted changes"
                    );
                    let names =
                        if sandbox::installed(sbx) { sandbox::in_worktree(home, sbx, &tree.path)? } else { Vec::new() };
                    for name in &names {
                        sandbox::stop(home, sbx, name)?;
                    }
                    git::remove_worktree(&repo, &tree.path, None, false)?;
                    for name in names {
                        sandbox::remove(home, sbx, &name)?;
                    }
                    Ok(())
                })();
                if let Err(err) = result {
                    notes.push(format!("Warm worktree {}: {err:#}", tree.path.display()));
                }
            }
            Ok(())
        })();
        if let Err(err) = result {
            notes.push(format!("Worktrees {}: {err:#}", repo.display()));
        }
    }
    Ok(notes)
}

/// Removes a scratch session's folder. Only a folder directly in `home`'s scratch folder is one.
pub fn remove_scratch(home: &Path, dir: &Path) -> Result<()> {
    anyhow::ensure!(
        dir.parent() == Some(home.join("scratch").as_path()) && dir.file_name().is_some(),
        "{} is not a scratch folder",
        dir.display()
    );
    match std::fs::remove_dir_all(dir) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(err).with_context(|| format!("removing {}", dir.display()))
        }
        _ => Ok(()),
    }
}

/// Removes scratch folders older than `before`, a file time such as [`Cutoff::files`], that no session
/// in `keep`, a restart's handoff, resumes. Their sessions ended when ATOMICS quit or crashed.
pub fn sweep_scratch(home: &Path, keep: &HashSet<PathBuf>, before: std::time::SystemTime) -> Vec<String> {
    let entries = match std::fs::read_dir(home.join("scratch")) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => return vec![format!("Scratch folders: {err}")],
    };
    let mut notes = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        let new = entry.metadata().and_then(|m| m.modified()).is_ok_and(|modified| modified >= before);
        if new || keep.contains(&dir) || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Err(err) = remove_scratch(home, &dir) {
            notes.push(format!("Scratch folder: {err:#}"));
        }
    }
    notes
}

/// Cache clones are staged in .atomics-warm-* directories beside their destination.
/// Only old stages in configured cache parents are ours; never follow cache symlinks.
fn sweep_warm_stages(root: &Path, folders: &[String], notes: &mut Vec<String>) -> Result<()> {
    let mut stages = Vec::new();
    for folder in folders {
        let relative = Path::new(folder);
        if relative.components().any(|part| !matches!(part, std::path::Component::Normal(name) if name != ".git")) {
            continue;
        }
        let parent = root.join(relative).parent().unwrap().to_path_buf();
        if !real(&parent).starts_with(real(root)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(parent) else { continue };
        for entry in entries {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(".atomics-warm-") || !entry.file_type()?.is_dir() {
                continue;
            }
            let old = entry.metadata()?.modified()?.elapsed().is_ok_and(|age| age.as_secs() > 86400);
            if old && !stages.contains(&entry.path()) {
                stages.push(entry.path());
            }
        }
    }
    if stages.is_empty() {
        return Ok(());
    }
    let mut args =
        vec!["status".to_string(), "--porcelain".into(), "--untracked-files=all".into(), "--".into(), ".".into()];
    for stage in &stages {
        args.push(format!(":(literal,exclude){}", stage.strip_prefix(root)?.display()));
    }
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let clean = git::git(root, &args)?.is_empty();
    for stage in stages {
        let result = if clean {
            std::fs::remove_dir_all(&stage).map_err(anyhow::Error::from)
        } else {
            Err(anyhow::anyhow!("worktree has uncommitted changes"))
        };
        if let Err(err) = result {
            notes.push(format!("Warm cache {}: {err:#}", stage.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::fake;
    use atomics_core::{
        plan::{PlanStatus, Task},
        store::SessionRecord,
    };

    fn repository(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-qb", "main"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.com"],
            vec!["commit", "--allow-empty", "-qm", "base"],
        ] {
            git::git(&repo, &args).unwrap();
        }
        repo
    }

    fn sandbox(bin: &Path, name: &str, worktree: &Path) {
        std::fs::create_dir_all(bin.join("sandboxes")).unwrap();
        std::fs::create_dir_all(bin.join("mcp")).unwrap();
        std::fs::write(
            bin.join("sandboxes").join(name),
            serde_json::json!({
                "name":name, "agent":"claude", "status":"running", "workspaces":[worktree]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(bin.join("mcp").join(name), "registration").unwrap();
    }

    #[test]
    fn two_homes_share_sbx_without_sweeping_each_others_resources_or_unknown_names() {
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        let a = dir.path().join("home-a");
        let b = dir.path().join("home-b");
        for (home, name) in [(&a, "atomics-a"), (&b, "atomics-b")] {
            sandbox(&bin, name, dir.path());
            ownership::created(home, name, Resource::Mcp).unwrap();
            ownership::created(home, name, Resource::Sandbox).unwrap();
        }
        sandbox(&bin, "atomics-bench-unknown", dir.path());
        std::fs::write(bin.join("mcp/atomics-unknown-mcp-only"), "keep").unwrap();
        assert!(sweep(&a, &Config::default(), &sbx).unwrap().is_empty());
        assert!(ownership::owned(&a).unwrap().is_empty());
        for folder in ["sandboxes", "mcp"] {
            assert!(!bin.join(folder).join("atomics-a").exists());
            assert!(bin.join(folder).join("atomics-b").exists());
            assert!(bin.join(folder).join("atomics-bench-unknown").exists());
        }
        let before = fake::calls(&bin);
        sandbox::stop(&a, &sbx, "atomics-b").unwrap();
        sandbox::remove(&a, &sbx, "atomics-b").unwrap();
        sandbox::remove_mcp(&a, &sbx, "atomics-unknown-mcp-only").unwrap();
        assert_eq!(fake::calls(&bin), before, "unowned resources must not even receive rm calls");
        let attempt = sandbox::Prepare {
            home: a.clone(),
            sbx: sbx.clone(),
            name: "atomics-b".into(),
            agent: "shell",
            cwd: dir.path().to_path_buf(),
            config: Default::default(),
            warm: Vec::new(),
            pins: a.join("git-pins"),
            exe: "/atomics".into(),
            socket: dir.path().join("unused.sock"),
            briefs: None,
            relay: None,
            secret_workspace: None,
            github_account: None,
        };
        assert!(sandbox::prepare(&attempt).unwrap_err().to_string().contains("not owned"));
        assert!(ownership::owned(&a).unwrap().is_empty());
        assert!(sweep(&b, &Config::default(), &sbx).unwrap().is_empty());
        assert!(ownership::owned(&b).unwrap().is_empty());
        assert!(!bin.join("sandboxes/atomics-b").exists() && !bin.join("mcp/atomics-b").exists());
        assert!(bin.join("mcp/atomics-unknown-mcp-only").exists());
        std::fs::write(a.join("sandbox-ownership.json"), "invalid ledger").unwrap();
        assert!(sweep(&a, &Config::default(), &sbx).is_err());
        assert!(bin.join("sandboxes/atomics-bench-unknown").exists());
    }

    #[test]
    fn missing_inventory_keeps_young_or_live_creation_reservations() {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(dir.path()).display().to_string();
        let home = dir.path().join("home");
        for name in ["young", "live", "dead"] {
            ownership::created(&home, name, Resource::Sandbox).unwrap();
        }
        let mut entries = ownership::owned(&home).unwrap();
        entries.get_mut("live").unwrap().created = 0;
        entries.get_mut("dead").unwrap().created = 0;
        // The live reservation is made by this process, while a dead creator can be absent.
        let mut json = serde_json::to_value(&entries).unwrap();
        json["live"]["creator"] = serde_json::json!(std::process::id());
        json["young"]["creator"] = serde_json::Value::Null;
        json["dead"]["creator"] = serde_json::Value::Null;
        std::fs::write(home.join("sandbox-ownership.json"), serde_json::to_vec(&json).unwrap()).unwrap();
        assert!(sandbox::remove(&home, &sbx, "young").is_err());
        assert!(sandbox::remove(&home, &sbx, "live").is_err());
        assert!(sweep(&home, &Config::default(), &sbx).unwrap().is_empty());
        let kept = ownership::owned(&home).unwrap();
        assert!(kept.contains_key("young") && kept.contains_key("live"), "{kept:?}");
        assert!(!kept.contains_key("dead"), "{kept:?}");
        assert!(fake::calls_to(dir.path(), &["rm"]).is_empty());
    }

    #[test]
    fn background_sweep_keeps_resources_and_briefs_created_after_its_cutoff() {
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        let home = dir.path().join("home");
        let before = Cutoff::mark(&home).unwrap();
        sandbox(&bin, "atomics-starting", dir.path());
        ownership::created(&home, "atomics-starting", Resource::Mcp).unwrap();
        ownership::created(&home, "atomics-starting", Resource::Sandbox).unwrap();
        std::fs::create_dir_all(home.join("briefs")).unwrap();
        std::fs::write(home.join("briefs/starting.md"), "being prepared").unwrap();
        assert!(sweep_before(&home, &Config::default(), &sbx, before).unwrap().is_empty());
        assert!(bin.join("sandboxes/atomics-starting").exists() && bin.join("mcp/atomics-starting").exists());
        assert!(home.join("briefs/starting.md").exists());
        assert!(fake::calls_to(&bin, &["rm"]).is_empty());
    }

    #[test]
    fn closed_plan_briefs_are_pruned_even_while_sessions_remain_in_history() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path();
        let mut plan = Plan {
            id: "closed".into(),
            status: PlanStatus::Completed,
            tasks: vec![Task { id: "task".into(), session: Some("pane".into()), ..Default::default() }],
            ..Default::default()
        };
        plan.save_in(home).unwrap();
        let record = SessionRecord {
            chief: false,
            pane: Some("pane".into()),
            claude_session_id: "conversation".into(),
            agent: "claude".into(),
            cwd: home.into(),
            title: "task".into(),
            last_prompt: None,
            updated: 0,
            sandbox: Some("atomics-pane".into()),
            named: false,
        };
        let store = Store { sessions: vec![record], ..Default::default() };
        store.save(&home.join("sessions.json")).unwrap();
        let brief = home.join("briefs/pane/brief.md");
        std::fs::create_dir_all(brief.parent().unwrap()).unwrap();
        std::fs::write(&brief, "old brief").unwrap();
        // Left by an earlier run: written just now, it could share the cutoff's clock tick, and so count as new.
        let backdate = || {
            let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            std::fs::File::open(brief.parent().unwrap()).unwrap().set_modified(earlier).unwrap();
        };
        // Open tasks retain their input even when there is no live terminal.
        plan.status = PlanStatus::Open;
        plan.save_in(home).unwrap();
        backdate();
        assert!(sweep(home, &Config::default(), "/missing/sbx").unwrap().is_empty());
        assert!(brief.exists());
        for status in [PlanStatus::Completed, PlanStatus::Archived] {
            plan.status = status;
            plan.save_in(home).unwrap();
            std::fs::write(&brief, "old brief").unwrap();
            backdate();
            assert!(sweep(home, &Config::default(), "/missing/sbx").unwrap().is_empty());
            assert!(!brief.exists());
            assert_eq!(Store::load(&home.join("sessions.json")).sessions.len(), 1);
            std::fs::create_dir_all(brief.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn startup_sweep_removes_only_orphans_and_clean_warm_worktrees() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path().join("home");
        let repo = repository(dir.path());
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        let clean = dir.path().join("warm");
        let dirty = dir.path().join("dirty");
        git::add_worktree(&repo, &clean, "atomics/warm-clean", "main").unwrap();
        git::add_worktree(&repo, &dirty, "atomics/warm-dirty", "main").unwrap();
        std::fs::write(dirty.join("keep"), "uncommitted").unwrap();
        sandbox(&bin, "atomics-live", &repo);
        sandbox(&bin, "atomics-orphan", &clean);
        ownership::created(&home, "atomics-orphan", Resource::Sandbox).unwrap();
        ownership::created(&home, "atomics-orphan", Resource::Mcp).unwrap();
        sandbox(&bin, "someone-else", &repo);
        std::fs::write(bin.join("mcp/atomics-mcp-only"), "stale").unwrap();
        ownership::created(&home, "atomics-mcp-only", Resource::Mcp).unwrap();
        let mut store = Store::default();
        store.sessions.push(SessionRecord {
            chief: false,
            pane: Some("live".into()),
            claude_session_id: "conversation".into(),
            agent: "claude".into(),
            cwd: repo.clone(),
            title: "live".into(),
            last_prompt: None,
            updated: 0,
            sandbox: Some("atomics-live".into()),
            named: false,
        });
        store.save(&home.join("sessions.json")).unwrap();
        std::fs::create_dir_all(home.join("briefs/live")).unwrap();
        std::fs::create_dir_all(home.join("briefs/stale")).unwrap();
        std::fs::write(home.join("briefs/live/brief.md"), "keep").unwrap();
        std::fs::write(home.join("briefs/stale/brief.md"), "remove").unwrap();
        std::fs::write(home.join("briefs/orphan.md"), "remove").unwrap();
        // Left by an earlier run: written just now, they could share the cutoff's clock tick, and so count as new.
        for path in [clean.clone(), dirty.clone(), home.join("briefs/stale"), home.join("briefs/orphan.md")] {
            let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            std::fs::File::open(path).unwrap().set_modified(earlier).unwrap();
        }
        let config = Config { repos: vec![repo.display().to_string()], ..Config::default() };
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(!clean.exists());
        assert!(dirty.join("keep").exists());
        assert!(home.join("briefs/live/brief.md").exists());
        assert!(!home.join("briefs/stale").exists() && !home.join("briefs/orphan.md").exists());
        assert!(!bin.join("sandboxes/atomics-orphan").exists() && !bin.join("mcp/atomics-orphan").exists());
        assert!(!bin.join("mcp/atomics-mcp-only").exists());
        assert!(bin.join("sandboxes/atomics-live").exists() && bin.join("mcp/atomics-live").exists());
        assert!(bin.join("sandboxes/someone-else").exists() && bin.join("mcp/someone-else").exists());
        assert!(notes.iter().any(|n| n.contains("uncommitted")));
        assert_eq!(notes, leftovers(&home));
        // A corrupt store must never turn live resources into apparent orphans.
        std::fs::write(home.join("sessions.json"), "broken").unwrap();
        assert!(sweep(&home, &config, &sbx).is_err());
        assert!(bin.join("sandboxes/atomics-live").exists());
    }

    #[test]
    fn plan_cleanup_removes_sandbox_and_registration_and_keeps_dirty_or_busy_work() {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = repository(dir.path());
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        let home = dir.path().join("home");
        let mut plan =
            Plan { id: "ship".into(), status: PlanStatus::Completed, project: Some(repo.clone()), ..Plan::default() };
        git::git(&repo, &["branch", &plan.integration_branch()]).unwrap();
        for branch in ["clean", "dirty", "busy"] {
            let path = dir.path().join(branch);
            git::add_worktree(&repo, &path, branch, "main").unwrap();
            plan.tasks.push(Task { branch: Some(branch.into()), ..Task::default() });
            sandbox(&bin, &format!("atomics-{branch}"), &path);
            ownership::created(&home, &format!("atomics-{branch}"), Resource::Sandbox).unwrap();
            ownership::created(&home, &format!("atomics-{branch}"), Resource::Mcp).unwrap();
        }
        std::fs::write(dir.path().join("dirty/keep"), "dirty").unwrap();
        let notes = clean_plan(&home, &plan, &sbx, &[dir.path().join("busy")]).notes;
        assert!(notes.iter().any(|n| n.contains("uncommitted")) && notes.iter().any(|n| n.contains("another session")));
        assert!(!dir.path().join("clean").exists());
        assert!(dir.path().join("dirty/keep").exists() && dir.path().join("busy").exists());
        assert!(!bin.join("sandboxes/atomics-clean").exists() && !bin.join("mcp/atomics-clean").exists());
        assert_eq!(fake::calls_to(&bin, &["stop"]), [["stop", "atomics-clean"]]);
    }

    #[test]
    fn cleanup_loads_pins_before_status_and_removal_of_a_redirected_worktree() {
        for redirect_common in [false, true] {
            for dirty in [false, true] {
                let dir = crate::test_support::tempdir().unwrap();
                let repo = repository(dir.path());
                let home = dir.path().join("home");
                let bin = dir.path().join("bin");
                std::fs::create_dir(&bin).unwrap();
                let sbx = fake::install(&bin).display().to_string();
                let tree = dir.path().join("task");
                git::add_worktree(&repo, &tree, "task", "main").unwrap();
                sandbox(&bin, "atomics-task", &tree);
                ownership::created(&home, "atomics-task", Resource::Sandbox).unwrap();
                ownership::created(&home, "atomics-task", Resource::Mcp).unwrap();
                let pins = home.join("git-pins");
                sandbox::pin_git(&pins, "atomics-task", &tree).unwrap();
                ownership::record_pins(&home, "atomics-task", &pins).unwrap();
                let evil = dir.path().join("evil");
                std::fs::create_dir(&evil).unwrap();
                git::git(&evil, &["init", "-q"]).unwrap();
                let marker = dir.path().join("executed-fsmonitor");
                git::git(&evil, &["config", "core.fsmonitor", &format!("touch {}", marker.display())]).unwrap();
                if redirect_common {
                    let admin = git::pinned(&tree).unwrap().git_dir.unwrap();
                    std::fs::write(admin.join("commondir"), evil.join(".git").display().to_string()).unwrap();
                } else {
                    std::fs::write(tree.join(".git"), format!("gitdir: {}\n", evil.join(".git").display())).unwrap();
                }
                if dirty {
                    std::fs::write(tree.join("keep"), "uncommitted").unwrap();
                }
                git::unpin(&tree.canonicalize().unwrap()); // Simulate a newly started host.
                let plan = Plan {
                    id: "pinned".into(),
                    status: PlanStatus::Archived,
                    project: Some(repo),
                    tasks: vec![Task { branch: Some("task".into()), ..Task::default() }],
                    ..Plan::default()
                };
                let notes = clean_plan(&home, &plan, &sbx, &[]).notes;
                assert!(!marker.exists(), "cleanup followed the sandbox's git config");
                if redirect_common {
                    assert!(tree.exists() && notes.iter().any(|note| note.contains("commondir")), "{notes:?}");
                    sandbox::remove(&home, &sbx, "atomics-task").unwrap();
                    let saved = git::pinned(&tree).unwrap();
                    assert!(!pins.join("atomics-task.json").exists());
                    // The refs backend cannot be trusted with a rewritten commondir. The folder
                    // stays pinned and cleanup leaves it until its metadata is repaired.
                    assert!(git::git(&tree, &["status"]).is_err());
                    std::fs::write(
                        saved.git_dir.as_ref().unwrap().join("commondir"),
                        saved.common_dir.as_ref().unwrap().display().to_string(),
                    )
                    .unwrap();
                    git::git(&tree, &["status", "--porcelain"]).unwrap();
                    assert!(!marker.exists());
                    git::unpin(&saved.work_tree);
                    continue;
                }
                assert_eq!(tree.exists(), dirty, "{notes:?}");
                if dirty {
                    assert!(notes.iter().any(|note| note.contains("uncommitted")));
                    sandbox::remove(&home, &sbx, "atomics-task").unwrap();
                    assert!(!pins.join("atomics-task.json").exists());
                    git::unpin(&tree.canonicalize().unwrap());
                    sandbox::load_pins(&pins).unwrap();
                    assert!(git::pinned(&tree).is_some(), "a poisoned folder must stay pinned after its VM is gone");
                    git::git(&tree, &["status", "--porcelain"]).unwrap();
                    assert!(!marker.exists());
                    git::unpin(&tree.canonicalize().unwrap());
                } else {
                    assert!(notes.is_empty(), "{notes:?}");
                    assert!(!pins.join("atomics-task.json").exists());
                }
                assert!(ownership::owned(&home).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn startup_sweep_never_enters_a_worktree_a_sandbox_lists_from_its_git_folder() {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = repository(dir.path());
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        let tree = dir.path().join("task");
        git::add_worktree(&repo, &tree, "atomics/warm-task", "main").unwrap();
        sandbox::pin_git(&home.join("git-pins"), "atomics-task", &tree).unwrap();
        // The VM plants a repository whose fsmonitor leaves `marker` in its worktree's read-write git
        // folder, names it in the folder's gitdir, and gives it an old cache stage for the sweep.
        let admin = git::pinned(&tree).unwrap().git_dir.unwrap();
        let evil = admin.join("evil");
        let marker = dir.path().join("executed-fsmonitor");
        let raw = |dir: &Path, args: &[&str]| std::process::Command::new("git").arg("-C").arg(dir).args(args).output();
        assert!(raw(&admin, &["init", "-q", "evil"]).unwrap().status.success());
        let touch = format!("touch {}", marker.display());
        assert!(raw(&evil, &["config", "core.fsmonitor", &touch]).unwrap().status.success());
        std::fs::write(admin.join("gitdir"), format!("{}\n", evil.join(".git").display())).unwrap();
        let stage = evil.join(".atomics-warm-old");
        std::fs::create_dir(&stage).unwrap();
        std::fs::File::open(&stage)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        git::unpin(&tree.canonicalize().unwrap()); // Simulate a newly started host.

        let config = Config { repos: vec![repo.display().to_string()], ..Config::default() };
        assert_eq!(config.worktrees.warm, ["target"]);
        sweep(&home, &config, &sbx).unwrap();
        assert!(!marker.exists(), "the sweep ran git in the sandbox's planted repository");
        assert!(stage.exists() && tree.exists());
        assert!(raw(&evil, &["status"]).unwrap().status.success() && marker.exists(), "the planted config is live");
        git::unpin(&tree.canonicalize().unwrap());
    }

    /// The ledger notes `name`'s git record in `pins` long ago, while the record file itself is gone.
    fn stale_pin_record(home: &Path, name: &str, sandbox: bool) -> PathBuf {
        let pins = home.join("git-pins");
        let folder = home.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        sandbox::pin_git(&pins, name, &folder).unwrap();
        if sandbox {
            ownership::created(home, name, Resource::Sandbox).unwrap();
        }
        ownership::record_pins(home, name, &pins).unwrap();
        git::unpin(&folder.canonicalize().unwrap());
        std::fs::remove_dir(&folder).unwrap();
        std::fs::remove_file(pins.join(format!("{name}.json"))).unwrap();
        let mut ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.join("sandbox-ownership.json")).unwrap()).unwrap();
        ledger[name]["created"] = 0.into();
        ledger[name]["creator"] = serde_json::Value::Null;
        std::fs::write(home.join("sandbox-ownership.json"), ledger.to_string()).unwrap();
        pins.join(format!("{name}.json"))
    }

    #[test]
    fn missing_git_records_of_gone_sandboxes_and_deleted_projects_heal_instead_of_failing_every_start() {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = repository(dir.path());
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        stale_pin_record(&home, "atomics-gone", true);
        stale_pin_record(&home, "atomics-unowned", false);
        let alive = stale_pin_record(&home, "atomics-alive", true);
        sandbox(&bin, "atomics-alive", &repo);
        // A session still names the live sandbox, so the sweep leaves it running.
        let scratch = dir.path().join("scratch-project");
        let store = Store {
            sessions: vec![SessionRecord {
                chief: false,
                pane: Some("alive".into()),
                claude_session_id: "conversation".into(),
                agent: "codex".into(),
                cwd: repo.clone(),
                title: "alive".into(),
                last_prompt: None,
                updated: 0,
                sandbox: Some("atomics-alive".into()),
                named: false,
            }],
            recent_projects: vec![scratch.clone()],
            ..Default::default()
        };
        store.save(&home.join("sessions.json")).unwrap();
        let config = Config { repos: vec![repo.display().to_string()], ..Config::default() };

        // While sbx still has a sandbox whose record is gone, cleanup fails closed and says what to do.
        let error = format!("{:#}", sweep(&home, &config, &sbx).unwrap_err());
        assert!(error.contains(&alive.display().to_string()) && error.contains("sbx rm atomics-alive"), "{error}");
        let owned = ownership::owned(&home).unwrap();
        assert!(owned["atomics-alive"].pins.is_some() && owned["atomics-alive"].sandbox, "{owned:?}");
        // Records of sandboxes that are gone pinned nothing; they no longer stop anything.
        assert!(!owned.contains_key("atomics-unowned") && owned["atomics-gone"].pins.is_none(), "{owned:?}");
        // Without an sbx to ask, the live sandbox's note stays too.
        assert!(sweep(&home, &config, "/missing/sbx").is_err());
        assert!(ownership::owned(&home).unwrap()["atomics-alive"].pins.is_some());

        // Once sbx no longer has it, the next start heals, and a deleted project folder is no failure.
        std::fs::remove_file(bin.join("sandboxes/atomics-alive")).unwrap();
        let earlier = [
            format!("Worktrees {}: git worktree list --porcelain failed: fatal: cannot change to", scratch.display()),
            format!("Startup cleanup skipped: reading {}: No such file or directory (os error 2)", alive.display()),
            format!("Plan cleanup skipped: reading {}: No such file or directory (os error 2)", alive.display()),
            format!("2{UNFINISHED_AT_QUIT}"),
            "Final ref guard check FAILED: refs/heads/main moved".to_string(),
        ];
        save_leftovers(&home, &earlier).unwrap();
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert!(ownership::owned(&home).unwrap().values().all(|entry| entry.pins.is_none()));
        sandbox::load_owned_pins(&home).unwrap();
        // The notes of earlier sweeps go; what no sweep looks at again stays.
        assert_eq!(leftovers(&home), ["Final ref guard check FAILED: refs/heads/main moved"]);

        // A failure the sweep finds is noted once and goes when it is fixed.
        let warm = dir.path().join("warm");
        git::add_worktree(&repo, &warm, "atomics/warm-dirty", "main").unwrap();
        std::fs::write(warm.join("keep"), "uncommitted").unwrap();
        let backdate = || {
            let earlier = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            std::fs::File::open(&warm).unwrap().set_modified(earlier).unwrap();
        };
        backdate();
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(
            notes.len() == 1 && notes[0].starts_with("Warm worktree") && notes[0].contains("uncommitted"),
            "{notes:?}"
        );
        assert_eq!(sweep(&home, &config, &sbx).unwrap(), notes, "the same failure is not added twice");
        assert_eq!(sweep_leftovers(&home), notes);
        std::fs::remove_file(warm.join("keep")).unwrap();
        backdate();
        assert!(sweep(&home, &config, &sbx).unwrap().is_empty());
        assert!(!warm.exists());
        assert_eq!(leftovers(&home), ["Final ref guard check FAILED: refs/heads/main moved"]);
        // A kept note recorded while a sweep ran is one it has not looked at.
        let before = kept_leftovers(&home);
        record_leftovers(&home, &["Scratch folder: removing /home/user/scratch/x: busy".into()]);
        record_sweep(&home, &[], true, &before);
        assert_eq!(kept_leftovers(&home).len(), 2);
    }

    #[test]
    fn startup_sweep_reports_failures_and_removes_old_cache_stages_only_in_clean_checkouts() {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = repository(dir.path());
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        sandbox(&bin, "atomics-stuck", &repo);
        ownership::created(&home, "atomics-stuck", Resource::Sandbox).unwrap();
        ownership::created(&home, "atomics-stuck", Resource::Mcp).unwrap();
        std::fs::write(bin.join("fail-rm"), "fail").unwrap();
        let stage = repo.join(".atomics-warm-old");
        std::fs::create_dir(&stage).unwrap();
        std::fs::File::open(&stage)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let config = Config { repos: vec![repo.display().to_string()], ..Config::default() };
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(!stage.exists());
        assert!(notes.iter().any(|n| n.contains("atomics-stuck")));
        let retained = ownership::owned(&home).unwrap();
        assert!(retained["atomics-stuck"].sandbox && retained["atomics-stuck"].mcp);
        assert_eq!(leftovers(&home), notes);
        std::fs::create_dir(&stage).unwrap();
        std::fs::File::open(&stage)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        std::fs::write(repo.join("dirty"), "keep").unwrap();
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(stage.exists() && notes.iter().any(|n| n.contains("uncommitted")));
    }
}
