//! Cleanup owned resources, with failures kept visible in doctor and the cleanup dialog.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use imacs_core::{cleanup, config::Config, git, plan::Plan, store::Store};

use crate::sandbox;
use crate::sandbox_ownership::{self as ownership, Resource};

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn clean_plan(home: &Path, plan: &Plan, sbx: &str, protected: &[PathBuf]) -> Vec<String> {
    let mut notes = Vec::new();
    if let Err(error) = sandbox::load_owned_pins(home) {
        return vec![format!("Plan cleanup skipped: {error:#}")];
    }
    let entries = match cleanup::closed_tasks(plan) {
        Ok(entries) => entries,
        Err(err) => return vec![format!("Plan cleanup: {err:#}")],
    };
    for entry in entries {
        let result = (|| -> Result<bool> {
            let mut names = Vec::new();
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
            removed_branch
        })();
        match result {
            Ok(false) if plan.status == imacs_core::plan::PlanStatus::Completed => {
                notes.push(format!("Kept branch {}: not merged into {}", entry.branch, plan.integration_branch()))
            }
            Ok(_) => {}
            Err(err) => notes.push(format!("Cleanup {}: {err:#}", entry.branch)),
        }
    }
    notes
}

/// Persisted failures survive quitting before a worker can deliver its toast.
pub fn leftovers(home: &Path) -> Vec<String> {
    std::fs::read(home.join("cleanup-leftovers.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save_leftovers(home: &Path, notes: &[String]) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let path = home.join("cleanup-leftovers.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(notes)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn record_leftovers(home: &Path, notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITE.lock().unwrap_or_else(|err| err.into_inner());
    let mut all = leftovers(home);
    for note in notes {
        if !all.contains(note) {
            all.push(note.clone());
        }
    }
    if let Err(err) = save_leftovers(home, &all) {
        eprintln!("IMACS: recording cleanup failures: {err:#}");
    }
}

/// Run only in the instance owning the control socket. The cutoff predates window construction's
/// return, so resources created while this background sweep runs cannot become candidates.
/// A malformed ownership record fails closed, rather than treating its resources as orphans.
#[cfg(test)]
pub fn sweep(home: &Path, config: &Config, sbx: &str) -> Result<Vec<String>> {
    let notes = sweep_before(home, config, sbx, std::time::SystemTime::now())?;
    record_leftovers(home, &notes);
    Ok(notes)
}

pub fn sweep_before(home: &Path, config: &Config, sbx: &str, before: std::time::SystemTime) -> Result<Vec<String>> {
    sandbox::load_owned_pins(home)?;
    let store = match std::fs::read(home.join("sessions.json")) {
        Ok(bytes) => serde_json::from_slice::<Store>(&bytes).context("reading cleanup session ownership")?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(err) => return Err(err.into()),
    };
    let (plans, errors) = imacs_core::plan::load_plans(home)?;
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
        .chain(sessions.iter().map(|id| format!("imacs-{id}")))
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
            if entry.metadata()?.modified()? >= before {
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
        .map(|r| imacs_core::paths::expand(r))
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
        let result = (|| -> Result<()> {
            let trees = git::list_worktrees(&repo)?;
            for tree in &trees {
                sweep_warm_stages(&tree.path, &config.worktrees.warm, &mut notes)?;
            }
            for tree in trees.into_iter().skip(1) {
                let warm = tree
                    .branch
                    .as_deref()
                    .is_some_and(|b| b.starts_with("imacs/warm-") || b.starts_with("imacs-warm-"));
                if !warm
                    || protected.contains(&real(&tree.path))
                    || std::fs::metadata(&tree.path).and_then(|m| m.modified()).is_ok_and(|modified| modified >= before)
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

/// Cache clones are staged in .imacs-warm-* directories beside their destination.
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
            if !entry.file_name().to_string_lossy().starts_with(".imacs-warm-") || !entry.file_type()?.is_dir() {
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
    use imacs_core::{
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
        for (home, name) in [(&a, "imacs-a"), (&b, "imacs-b")] {
            sandbox(&bin, name, dir.path());
            ownership::created(home, name, Resource::Mcp).unwrap();
            ownership::created(home, name, Resource::Sandbox).unwrap();
        }
        sandbox(&bin, "imacs-bench-unknown", dir.path());
        std::fs::write(bin.join("mcp/imacs-unknown-mcp-only"), "keep").unwrap();
        assert!(sweep(&a, &Config::default(), &sbx).unwrap().is_empty());
        assert!(ownership::owned(&a).unwrap().is_empty());
        for folder in ["sandboxes", "mcp"] {
            assert!(!bin.join(folder).join("imacs-a").exists());
            assert!(bin.join(folder).join("imacs-b").exists());
            assert!(bin.join(folder).join("imacs-bench-unknown").exists());
        }
        let before = fake::calls(&bin);
        sandbox::stop(&a, &sbx, "imacs-b").unwrap();
        sandbox::remove(&a, &sbx, "imacs-b").unwrap();
        sandbox::remove_mcp(&a, &sbx, "imacs-unknown-mcp-only").unwrap();
        assert_eq!(fake::calls(&bin), before, "unowned resources must not even receive rm calls");
        let attempt = sandbox::Prepare {
            home: a.clone(),
            sbx: sbx.clone(),
            name: "imacs-b".into(),
            agent: "shell",
            cwd: dir.path().to_path_buf(),
            config: Default::default(),
            warm: Vec::new(),
            pins: a.join("git-pins"),
            exe: "/imacs".into(),
            socket: dir.path().join("unused.sock"),
            briefs: None,
            relay: None,
        };
        assert!(sandbox::prepare(&attempt).unwrap_err().to_string().contains("not owned"));
        assert!(ownership::owned(&a).unwrap().is_empty());
        assert!(sweep(&b, &Config::default(), &sbx).unwrap().is_empty());
        assert!(ownership::owned(&b).unwrap().is_empty());
        assert!(!bin.join("sandboxes/imacs-b").exists() && !bin.join("mcp/imacs-b").exists());
        assert!(bin.join("mcp/imacs-unknown-mcp-only").exists());
        std::fs::write(a.join("sandbox-ownership.json"), "invalid ledger").unwrap();
        assert!(sweep(&a, &Config::default(), &sbx).is_err());
        assert!(bin.join("sandboxes/imacs-bench-unknown").exists());
    }

    #[test]
    fn background_sweep_keeps_resources_and_briefs_created_after_its_cutoff() {
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        let home = dir.path().join("home");
        let before = std::time::SystemTime::now();
        sandbox(&bin, "imacs-starting", dir.path());
        ownership::created(&home, "imacs-starting", Resource::Mcp).unwrap();
        ownership::created(&home, "imacs-starting", Resource::Sandbox).unwrap();
        std::fs::create_dir_all(home.join("briefs")).unwrap();
        std::fs::write(home.join("briefs/starting.md"), "being prepared").unwrap();
        assert!(sweep_before(&home, &Config::default(), &sbx, before).unwrap().is_empty());
        assert!(bin.join("sandboxes/imacs-starting").exists() && bin.join("mcp/imacs-starting").exists());
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
            sandbox: Some("imacs-pane".into()),
        };
        let store = Store { sessions: vec![record], ..Default::default() };
        store.save(&home.join("sessions.json")).unwrap();
        let brief = home.join("briefs/pane/brief.md");
        std::fs::create_dir_all(brief.parent().unwrap()).unwrap();
        std::fs::write(&brief, "old brief").unwrap();
        // Open tasks retain their input even when there is no live terminal.
        plan.status = PlanStatus::Open;
        plan.save_in(home).unwrap();
        assert!(sweep(home, &Config::default(), "/missing/sbx").unwrap().is_empty());
        assert!(brief.exists());
        for status in [PlanStatus::Completed, PlanStatus::Archived] {
            plan.status = status;
            plan.save_in(home).unwrap();
            std::fs::write(&brief, "old brief").unwrap();
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
        git::add_worktree(&repo, &clean, "imacs/warm-clean", "main").unwrap();
        git::add_worktree(&repo, &dirty, "imacs/warm-dirty", "main").unwrap();
        std::fs::write(dirty.join("keep"), "uncommitted").unwrap();
        sandbox(&bin, "imacs-live", &repo);
        sandbox(&bin, "imacs-orphan", &clean);
        ownership::created(&home, "imacs-orphan", Resource::Sandbox).unwrap();
        ownership::created(&home, "imacs-orphan", Resource::Mcp).unwrap();
        sandbox(&bin, "someone-else", &repo);
        std::fs::write(bin.join("mcp/imacs-mcp-only"), "stale").unwrap();
        ownership::created(&home, "imacs-mcp-only", Resource::Mcp).unwrap();
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
            sandbox: Some("imacs-live".into()),
        });
        store.save(&home.join("sessions.json")).unwrap();
        std::fs::create_dir_all(home.join("briefs/live")).unwrap();
        std::fs::create_dir_all(home.join("briefs/stale")).unwrap();
        std::fs::write(home.join("briefs/live/brief.md"), "keep").unwrap();
        std::fs::write(home.join("briefs/stale/brief.md"), "remove").unwrap();
        std::fs::write(home.join("briefs/orphan.md"), "remove").unwrap();
        let config = Config { repos: vec![repo.display().to_string()], ..Config::default() };
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(!clean.exists());
        assert!(dirty.join("keep").exists());
        assert!(home.join("briefs/live/brief.md").exists());
        assert!(!home.join("briefs/stale").exists() && !home.join("briefs/orphan.md").exists());
        assert!(!bin.join("sandboxes/imacs-orphan").exists() && !bin.join("mcp/imacs-orphan").exists());
        assert!(!bin.join("mcp/imacs-mcp-only").exists());
        assert!(bin.join("sandboxes/imacs-live").exists() && bin.join("mcp/imacs-live").exists());
        assert!(bin.join("sandboxes/someone-else").exists() && bin.join("mcp/someone-else").exists());
        assert!(notes.iter().any(|n| n.contains("uncommitted")));
        assert_eq!(notes, leftovers(&home));
        // A corrupt store must never turn live resources into apparent orphans.
        std::fs::write(home.join("sessions.json"), "broken").unwrap();
        assert!(sweep(&home, &config, &sbx).is_err());
        assert!(bin.join("sandboxes/imacs-live").exists());
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
            sandbox(&bin, &format!("imacs-{branch}"), &path);
            ownership::created(&home, &format!("imacs-{branch}"), Resource::Sandbox).unwrap();
            ownership::created(&home, &format!("imacs-{branch}"), Resource::Mcp).unwrap();
        }
        std::fs::write(dir.path().join("dirty/keep"), "dirty").unwrap();
        let notes = clean_plan(&home, &plan, &sbx, &[dir.path().join("busy")]);
        assert!(notes.iter().any(|n| n.contains("uncommitted")) && notes.iter().any(|n| n.contains("another session")));
        assert!(!dir.path().join("clean").exists());
        assert!(dir.path().join("dirty/keep").exists() && dir.path().join("busy").exists());
        assert!(!bin.join("sandboxes/imacs-clean").exists() && !bin.join("mcp/imacs-clean").exists());
        assert_eq!(fake::calls_to(&bin, &["stop"]), [["stop", "imacs-clean"]]);
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
                sandbox(&bin, "imacs-task", &tree);
                ownership::created(&home, "imacs-task", Resource::Sandbox).unwrap();
                ownership::created(&home, "imacs-task", Resource::Mcp).unwrap();
                let pins = home.join("git-pins");
                sandbox::pin_git(&pins, "imacs-task", &tree).unwrap();
                ownership::record_pins(&home, "imacs-task", &pins).unwrap();
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
                let notes = clean_plan(&home, &plan, &sbx, &[]);
                assert!(!marker.exists(), "cleanup followed the sandbox's git config");
                assert_eq!(tree.exists(), dirty, "{notes:?}");
                if dirty {
                    assert!(notes.iter().any(|note| note.contains("uncommitted")));
                    sandbox::remove(&home, &sbx, "imacs-task").unwrap();
                    assert!(!pins.join("imacs-task.json").exists());
                    git::unpin(&tree.canonicalize().unwrap());
                    sandbox::load_pins(&pins);
                    assert!(git::pinned(&tree).is_some(), "a poisoned folder must stay pinned after its VM is gone");
                    git::git(&tree, &["status", "--porcelain"]).unwrap();
                    assert!(!marker.exists());
                    git::unpin(&tree.canonicalize().unwrap());
                } else {
                    assert!(notes.is_empty(), "{notes:?}");
                    assert!(!pins.join("imacs-task.json").exists());
                }
                assert!(ownership::owned(&home).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn startup_sweep_reports_failures_and_removes_old_cache_stages_only_in_clean_checkouts() {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = repository(dir.path());
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sbx = fake::install(&bin).display().to_string();
        sandbox(&bin, "imacs-stuck", &repo);
        ownership::created(&home, "imacs-stuck", Resource::Sandbox).unwrap();
        ownership::created(&home, "imacs-stuck", Resource::Mcp).unwrap();
        std::fs::write(bin.join("fail-rm"), "fail").unwrap();
        let stage = repo.join(".imacs-warm-old");
        std::fs::create_dir(&stage).unwrap();
        std::fs::File::open(&stage)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let config = Config { repos: vec![repo.display().to_string()], ..Config::default() };
        let notes = sweep(&home, &config, &sbx).unwrap();
        assert!(!stage.exists());
        assert!(notes.iter().any(|n| n.contains("imacs-stuck")));
        let retained = ownership::owned(&home).unwrap();
        assert!(retained["imacs-stuck"].sandbox && retained["imacs-stuck"].mcp);
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
