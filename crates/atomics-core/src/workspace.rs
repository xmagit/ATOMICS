//! Projects and workspaces. A project is one git repository, always shown through
//! its main checkout. A workspace is a named set of projects, saved as
//! `~/.config/atomics/workspaces/<host>/<name>.toml`.

#[cfg(feature = "native")]
use std::path::Path;
use std::path::PathBuf;

#[cfg(feature = "native")]
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use crate::{git, paths};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(skip)]
    pub name: String,
    pub projects: Vec<PathBuf>,
    /// Preserve workspace-level settings such as `[tickets]` without interpreting them.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Workspace {
    pub fn identity(&self) -> Result<Option<crate::identity::Identity>, toml::de::Error> {
        Self::identity_in(&self.extra)
    }

    /// Determine identity without deserializing unrelated workspace settings.
    pub fn identity_in(table: &toml::Table) -> Result<Option<crate::identity::Identity>, toml::de::Error> {
        let identity: Option<crate::identity::Identity> =
            table.get("identity").map(|section| section.clone().try_into()).transpose()?;
        if let Some(identity) = &identity {
            identity.validate().map_err(<toml::de::Error as serde::de::Error>::custom)?;
        }
        Ok(identity)
    }

    /// This workspace's ticket system, read from its `[tickets]`. The section stays in
    /// `extra` as written, so saving never drops, rewrites or duplicates it.
    pub fn tickets(&self) -> Result<Option<crate::tickets::TicketConfig>, toml::de::Error> {
        self.extra.get("tickets").map(|section| section.clone().try_into()).transpose()
    }

    pub fn land(&self) -> Result<Option<crate::config::LandConfig>, toml::de::Error> {
        self.extra.get("land").map(|section| section.clone().try_into()).transpose()
    }

    pub fn workflow(&self) -> Result<Option<crate::workflow::Workflow>, toml::de::Error> {
        self.extra.get("workflow").map(|section| section.clone().try_into()).transpose()
    }

    /// This workspace's `[chief] cwd`, `resume` and `autostart`.
    pub fn chief(&self) -> Result<Option<crate::config::ChiefLaunch>, toml::de::Error> {
        self.extra.get("chief").map(|section| section.clone().try_into()).transpose()
    }

    /// This workspace's `[isolation]`. Unlike `[tickets]` it merges key by key over the main
    /// and host config's, with [`crate::isolation::IsolationConfig::with_overrides`].
    pub fn isolation(&self) -> Result<Option<crate::isolation::IsolationOverrides>, toml::de::Error> {
        let section = self.extra.get("isolation").map(|section| section.clone().try_into()).transpose()?;
        Ok(section.map(crate::isolation::IsolationOverrides))
    }
}

#[cfg(feature = "native")]
fn workspaces_root() -> PathBuf {
    paths::config_file().parent().map(Path::to_path_buf).unwrap_or_else(paths::atomics_home).join("workspaces")
}

#[cfg(feature = "native")]
pub fn workspaces_dir() -> PathBuf {
    workspaces_root().join(paths::host_name())
}

/// Only host configuration and an explicitly saved workspace register Git projects.
/// Opening or browsing a directory, session history and repository files confer no trust.
#[cfg(feature = "native")]
pub fn register_git_projects(config: &crate::config::Config, saved: Option<&Workspace>) {
    git::set_registered_projects(
        config
            .repos
            .iter()
            .map(|path| paths::expand(path))
            .chain(saved.into_iter().flat_map(|ws| ws.projects.iter().cloned())),
    );
}

/// Default depth when discovering git repositories beneath a non-repo working folder:
/// depth 1 matches direct children (`~/work/app`), depth 2 matches grouped folders
/// (`~/work/repos/app`).
#[cfg(feature = "native")]
pub const DEFAULT_DISCOVERY_DEPTH: usize = 2;

/// Non-hidden folder names skipped during working-folder repository discovery (build outputs
/// and dependency trees, matching the file watcher's ignore list).
#[cfg(feature = "native")]
const DISCOVERY_SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "bin", "obj"];

/// The project a folder belongs to: the main checkout of its repository, so a
/// folder inside any worktree opens the same project. Never one in an [`untrusted`] folder, where no git runs.
#[cfg(feature = "native")]
pub fn project_root(dir: &Path) -> Result<PathBuf> {
    if let Some(why) = untrusted(dir) {
        bail!("{why}");
    }
    let toplevel =
        git::worktree_root(dir).with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    let root = git::main_checkout(&toplevel).unwrap_or(toplevel);
    if let Some(why) = untrusted(&root) {
        bail!("{why}");
    }
    Ok(root)
}

/// Whether `dir` lies inside a `.git` directory (by path spelling or canonical path).
#[cfg(feature = "native")]
fn in_git_dir(dir: &Path) -> bool {
    let has_git = |path: &Path| path.components().any(|component| component.as_os_str() == ".git");
    has_git(dir) || dir.canonicalize().ok().is_some_and(|real| has_git(&real))
}

/// Whether `dir` is `/`, the home folder or a folder above it: where launchers and services start,
/// never a working folder of repositories.
#[cfg(feature = "native")]
fn home_or_above(dir: &Path) -> bool {
    let real = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dir = real(dir);
    dir.parent().is_none() || real(&paths::home()).starts_with(&dir)
}

/// Whether `dir` is a candidate non-repo working folder to scan for repositories.
#[cfg(feature = "native")]
pub fn can_discover(dir: &Path) -> bool {
    dir.is_dir() && !home_or_above(dir) && untrusted(dir).is_none() && !in_git_dir(dir)
}

#[cfg(feature = "native")]
fn skip_discovery_name(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().starts_with(b".") || DISCOVERY_SKIP_DIRS.iter().any(|ignored| name == *ignored)
}

/// Discovers git repositories beneath a non-repo working folder up to `max_depth` levels deep
/// (depth 1 is direct children, depth 2 is grandchildren). Hidden folders, build/dependency
/// directories (`target`, `node_modules`, `dist`, `bin`, `obj`), `.git` internals and
/// [`untrusted`] folders are skipped. Every candidate is mapped through [`project_root`] so
/// linked worktrees and symlink aliases deduplicate to one main checkout.
#[cfg(feature = "native")]
pub fn discover_projects(folder: &Path, max_depth: usize) -> Vec<PathBuf> {
    if max_depth == 0 || !can_discover(folder) {
        return Vec::new();
    }
    let candidates = discover_candidates(folder, max_depth);
    if candidates.is_empty() {
        return Vec::new();
    }
    let resolved = resolve_candidates(&candidates);
    let mut seen = std::collections::HashSet::new();
    let mut projects = Vec::new();
    for root in resolved {
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            projects.push(root);
        }
    }
    projects.sort();
    projects
}

#[cfg(feature = "native")]
fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        meta.is_dir().then(|| (meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(feature = "native")]
fn discover_candidates(folder: &Path, max_depth: usize) -> Vec<PathBuf> {
    use std::collections::{HashSet, VecDeque};

    let mut visited = HashSet::new();
    if let Some(id) = dir_identity(folder) {
        visited.insert(id);
    }
    let mut queue = VecDeque::from([(folder.to_path_buf(), 0usize)]);
    let mut candidates = Vec::new();

    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut children: Vec<(std::ffi::OsString, PathBuf)> =
            entries.flatten().map(|entry| (entry.file_name(), entry.path())).collect();
        children.sort_by(|(a, _), (b, _)| a.cmp(b));

        for (name, path) in children {
            if skip_discovery_name(&name) {
                continue;
            }
            // Follow symlinks via metadata so symlinked repos and container folders are inspected;
            // broken symlinks and non-directories are skipped without running git.
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if !meta.is_dir() {
                continue;
            }
            if untrusted(&path).is_some() || in_git_dir(&path) {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if !visited.insert((meta.dev(), meta.ino())) {
                    continue;
                }
            }
            if path.join(".git").symlink_metadata().is_ok() {
                candidates.push(path);
                continue;
            }
            let next_depth = depth + 1;
            if next_depth < max_depth {
                queue.push_back((path, next_depth));
            }
        }
    }
    candidates
}

#[cfg(feature = "native")]
fn resolve_candidates(candidates: &[PathBuf]) -> Vec<PathBuf> {
    const PARALLEL_THRESHOLD: usize = 4;
    const MAX_WORKERS: usize = 8;

    if candidates.len() <= PARALLEL_THRESHOLD {
        return candidates.iter().filter_map(|path| project_root(path).ok()).collect();
    }
    let workers =
        std::thread::available_parallelism().map(usize::from).unwrap_or(1).clamp(1, MAX_WORKERS).min(candidates.len());
    if workers <= 1 {
        return candidates.iter().filter_map(|path| project_root(path).ok()).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Vec<(usize, PathBuf)>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut local = Vec::new();
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(path) = candidates.get(index) else { break };
                    if let Ok(root) = project_root(path) {
                        local.push((index, root));
                    }
                }
                if !local.is_empty() {
                    results.lock().unwrap_or_else(|e| e.into_inner()).extend(local);
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, root)| root).collect()
}

/// A folder something this machine does not trust can write, and why it is distrusted.
#[cfg(feature = "native")]
struct Untrusted {
    folder: PathBuf,
    /// Device and inode, which also find it under another spelling: APFS ignores case.
    id: Option<(u64, u64)>,
    reason: String,
}

#[cfg(feature = "native")]
static UNTRUSTED: std::sync::LazyLock<std::sync::Mutex<Vec<Untrusted>>> = std::sync::LazyLock::new(Default::default);

/// From now on, this process treats `folder` as data from an untrusted source: never a project, no git in it
/// (`git::refuse_git`), and no agent starts there. On a Mac that is the VM's client, ATOMICS distrusts the
/// folders the VM can write (docs/nixos-vm.md, The exchange folder). `reason` completes "<folder> is …" and
/// "<dir> is in <folder>, …".
#[cfg(feature = "native")]
pub fn distrust(folder: &Path, reason: &str) {
    use std::os::unix::fs::MetadataExt;
    let mut untrusted = UNTRUSTED.lock().unwrap_or_else(|e| e.into_inner());
    for folder in [Some(folder.to_path_buf()), folder.canonicalize().ok()].into_iter().flatten() {
        if untrusted.iter().any(|known| known.folder == folder) {
            continue;
        }
        let pins = git::GitPins { work_tree: folder.clone(), git_dir: None, common_dir: None };
        git::refuse_git(&folder, Some(&pins), format!("{} is {reason}", folder.display()));
        let id = std::fs::metadata(&folder).ok().map(|meta| (meta.dev(), meta.ino()));
        untrusted.push(Untrusted { folder, id, reason: reason.to_string() });
    }
}

/// Why `dir` is in a folder this process distrusts ([`distrust`]): by its path as given, as it resolves, or as the
/// same folder on disk under another spelling.
#[cfg(feature = "native")]
pub fn untrusted(dir: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let untrusted = UNTRUSTED.lock().unwrap_or_else(|e| e.into_inner());
    if untrusted.is_empty() {
        return None;
    }
    let real = dir.canonicalize().ok();
    let found = untrusted
        .iter()
        .find(|known| {
            dir.starts_with(&known.folder) || real.as_ref().is_some_and(|real| real.starts_with(&known.folder))
        })
        .or_else(|| {
            real.as_ref()?.ancestors().find_map(|ancestor| {
                let meta = std::fs::metadata(ancestor).ok()?;
                untrusted.iter().find(|known| known.id == Some((meta.dev(), meta.ino())))
            })
        })?;
    Some(if dir == found.folder {
        format!("{} is {}", dir.display(), found.reason)
    } else {
        format!("{} is in {}, {}", dir.display(), found.folder.display(), found.reason)
    })
}

#[cfg(feature = "native")]
impl Workspace {
    pub fn load_identity(name: &str) -> Result<Option<crate::identity::Identity>> {
        let text = match std::fs::read_to_string(Self::file(name)?) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let table = toml::from_str::<toml::Table>(&text)?;
        Ok(Self::identity_in(&table)?)
    }

    pub fn load(name: &str) -> Result<Workspace> {
        let file = Self::file(name)?;
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let mut workspace: Workspace = toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        workspace.name = name.to_string();
        workspace.projects = workspace.projects.iter().map(|p| paths::expand(&p.to_string_lossy())).collect();
        Ok(workspace)
    }

    /// The file `load` reads: this host's, else a legacy flat file, else where `save` writes.
    pub fn file(name: &str) -> Result<PathBuf> {
        let filename = format!("{}.toml", file_name(name)?);
        let host = workspaces_dir().join(&filename);
        let legacy = workspaces_root().join(filename);
        Ok(if host.exists() || !legacy.exists() { host } else { legacy })
    }

    pub fn save(&self) -> Result<PathBuf> {
        let dir = workspaces_dir();
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{}.toml", file_name(&self.name)?));
        let mut portable = self.clone();
        portable.projects = self.projects.iter().map(|p| PathBuf::from(paths::display(p))).collect();
        std::fs::write(&file, toml::to_string(&portable)?)?;
        Ok(file)
    }

    /// Host and legacy workspace names, sorted with duplicates removed.
    pub fn list() -> Vec<String> {
        let mut names: Vec<String> = [workspaces_dir(), workspaces_root()]
            .into_iter()
            .flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten())
            .flatten()
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .filter(|_| e.path().extension().is_some_and(|x| x == "toml"))
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(feature = "native")]
fn file_name(name: &str) -> Result<String> {
    let clean = git::slug(name.trim());
    if clean.is_empty() {
        bail!("a workspace needs a name");
    }
    Ok(clean)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn legacy_workspaces_load_list_and_save_without_modifying_shared_files() {
        const CHILD: &str = "ATOMICS_LEGACY_WORKSPACE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "workspace::tests::legacy_workspaces_load_list_and_save_without_modifying_shared_files",
                    ])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("ATOMICS_HOST", "work-mac")
                    .env("ATOMICS_CONFIG", dir.path().join("config/config.toml")),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = workspaces_root();
        std::fs::create_dir_all(&root).unwrap();
        let legacy_file = root.join("backend.toml");
        let legacy_text = "projects = ['~/repos/backend']\n[tickets]\nprovider = 'linear'\n";
        std::fs::write(&legacy_file, legacy_text).unwrap();
        assert_eq!(Workspace::list(), ["backend"]);
        let mut workspace = Workspace::load("backend").unwrap();
        assert_eq!(workspace.projects, [paths::home().join("repos/backend")]);
        workspace.projects = vec![paths::home().join("repos/host-project")];
        let host_file = workspace.save().unwrap();
        assert_eq!(host_file, workspaces_dir().join("backend.toml"));
        assert_eq!(std::fs::read_to_string(&legacy_file).unwrap(), legacy_text);
        assert_eq!(Workspace::list(), ["backend"]);
        let host_workspace = Workspace::load("backend").unwrap();
        assert_eq!(host_workspace.projects, workspace.projects);
        assert_eq!(host_workspace.extra, workspace.extra);

        std::fs::write(root.join("legacy-only.toml"), "projects = []").unwrap();
        std::fs::write(workspaces_dir().join("host-only.toml"), "projects = []").unwrap();
        std::fs::create_dir(root.join("other-host")).unwrap();
        std::fs::write(root.join("other-host/other.toml"), "projects = []").unwrap();
        assert_eq!(Workspace::list(), ["backend", "host-only", "legacy-only"]);
        // An invalid host file must not silently load a different workspace.
        std::fs::write(&host_file, "[broken").unwrap();
        let error = format!("{:#}", Workspace::load("backend").unwrap_err());
        assert!(error.contains(&host_file.display().to_string()), "{error}");
        assert_eq!(std::fs::read_to_string(&legacy_file).unwrap(), legacy_text);
        std::fs::remove_file(host_file).unwrap();
        assert_eq!(Workspace::load("backend").unwrap().projects, [paths::home().join("repos/backend")]);
    }

    #[test]
    fn workspaces_are_host_local_and_preserve_portable_paths_and_ticket_settings() {
        const CHILD: &str = "ATOMICS_WORKSPACE_HOST_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            for host in ["work-mac", "home-linux"] {
                let folder = dir.path().join("config/workspaces").join(host);
                std::fs::create_dir_all(&folder).unwrap();
                std::fs::write(
                    folder.join("daily.toml"),
                    format!(
                        r#"projects = ["~/repos/{host}", "~"]
[tickets]
provider = "{host}"
workflow = "keep-this"
[tickets.options]
custom = true
"#
                    ),
                )
                .unwrap();
                std::fs::write(folder.join(format!("{host}.toml")), "projects = []").unwrap();
            }
            for host in ["work-mac", "home-linux"] {
                let output = crate::test_support::output(
                    Command::new(std::env::current_exe().unwrap())
                        .args(["--exact", "workspace::tests::workspaces_are_host_local_and_preserve_portable_paths_and_ticket_settings"])
                        .env(CHILD, "1").env("HOME", dir.path()).env("ATOMICS_HOST", host)
                        .env("ATOMICS_CONFIG", dir.path().join("config/config.toml")),
                );
                assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let host = std::env::var("ATOMICS_HOST").unwrap();
        assert_eq!(workspaces_dir(), paths::home().join("config/workspaces").join(&host));
        let mut expected = vec!["daily".to_string(), host.clone()];
        expected.sort();
        assert_eq!(Workspace::list(), expected);
        let mut workspace = Workspace::load("daily").unwrap();
        assert_eq!(workspace.projects, [paths::home().join("repos").join(&host), paths::home()]);
        assert_eq!(workspace.extra["tickets"]["provider"].as_str(), Some(host.as_str()));
        let settings = workspace.extra.clone();
        workspace.projects.push(paths::home().join("repos/another"));
        let file = workspace.save().unwrap();
        assert_eq!(file, workspaces_dir().join("daily.toml"));
        let text = std::fs::read_to_string(file).unwrap();
        assert!(text.contains("~/repos/another"), "{text}");
        assert!(!text.contains(&paths::home().display().to_string()), "{text}");
        let reloaded = Workspace::load("daily").unwrap();
        assert_eq!(reloaded.projects, workspace.projects);
        assert_eq!(reloaded.extra, settings);
    }

    #[test]
    fn saving_keeps_one_tickets_section_that_still_parses() {
        const CHILD: &str = "ATOMICS_WORKSPACE_TICKETS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "workspace::tests::saving_keeps_one_tickets_section_that_still_parses"])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("ATOMICS_HOST", "work-mac")
                    .env("ATOMICS_CONFIG", dir.path().join("config/config.toml")),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let tickets_header = |text: &str| text.lines().filter(|line| line.trim() == "[tickets]").count();
        // A legacy flat file is read, and the first save moves it to this host's folder.
        std::fs::create_dir_all(workspaces_root()).unwrap();
        let legacy = workspaces_root().join("team.toml");
        std::fs::write(
            &legacy,
            r#"projects = ["~/repos/app"]
[tickets]
system = "alpha"
mode = "ticket"
unknown_option = "kept"
server = { command = ["alpha-mcp"], env = { PROFILE = "work" } }
states = { running = "Doing", done = "Done" }
[tickets.operations.get]
tool = "read"
arguments = { id = "{key}", fields = ["id", "status"] }
result = { key = "id", state = "status" }
"#,
        )
        .unwrap();
        assert_eq!(Workspace::file("team").unwrap(), legacy);
        let loaded = Workspace::load("team").unwrap();
        let typed = loaded.tickets().unwrap().unwrap();
        assert_eq!((typed.system.as_str(), typed.mode), ("alpha", crate::tickets::TicketMode::Ticket));
        assert_eq!(typed.server.env["PROFILE"], "work");
        assert_eq!(typed.operation("get").unwrap().result.state, "status");
        let mut workspace = loaded.clone();
        workspace.projects.push(paths::home().join("repos/api"));
        for _ in 0..2 {
            let file = workspace.save().unwrap();
            assert_eq!(file, workspaces_dir().join("team.toml"));
            assert_eq!(Workspace::file("team").unwrap(), file);
            let text = std::fs::read_to_string(&file).unwrap();
            assert_eq!(tickets_header(&text), 1, "{text}");
            assert_eq!(text.matches("system = \"alpha\"").count(), 1, "{text}");
            let reloaded = Workspace::load("team").unwrap();
            assert_eq!(reloaded.projects, workspace.projects);
            assert_eq!(reloaded.extra, loaded.extra, "unknown keys survive too");
            assert_eq!(reloaded.tickets().unwrap(), Some(typed.clone()));
            workspace = reloaded;
        }
        assert!(Workspace { extra: toml::Table::new(), ..workspace.clone() }.tickets().unwrap().is_none());
        workspace.extra.insert("tickets".into(), toml::Value::String("not a table".into()));
        assert!(workspace.tickets().is_err());
    }

    #[test]
    fn worktree_folders_resolve_to_the_main_checkout() {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir(&repo).unwrap();
        let run =
            |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(args).status().unwrap().success());
        run(&["init", "-q", "-b", "main"]);
        run(&["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "base"]);
        let worktree = dir.path().join("app-feature");
        git::add_worktree(&repo, &worktree, "feature", "main").unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();

        let main = project_root(&repo).unwrap();
        assert_eq!(project_root(&worktree.join("src")).unwrap(), main);
        assert!(project_root(dir.path()).is_err());
    }

    #[test]
    fn a_distrusted_folder_is_never_a_project_under_any_spelling() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Distrust lasts for the process and also matches by inode: keep the folder until the process exits,
        // or a later test's folder could get its inode and be refused.
        std::mem::forget(dir);
        let (shared, trusted) = (root.join("shared"), root.join("trusted"));
        for repo in [shared.join("app"), trusted.join("app")] {
            std::fs::create_dir_all(&repo).unwrap();
            assert!(Command::new("git").args(["init", "-q"]).arg(&repo).status().unwrap().success());
        }
        std::os::unix::fs::symlink(shared.join("app"), trusted.join("link")).unwrap();
        distrust(&shared, "written by something else");
        assert_eq!(untrusted(&shared).unwrap(), format!("{} is written by something else", shared.display()));
        let error = project_root(&shared.join("app")).unwrap_err().to_string();
        assert_eq!(
            error,
            format!("{} is in {}, written by something else", shared.join("app").display(), shared.display())
        );
        assert!(project_root(&trusted.join("link")).is_err(), "a link into it leads into it");
        assert!(git::git(&shared.join("app"), &["status"]).is_err());
        let shouting = root.join("SHARED");
        if shouting.exists() {
            assert!(project_root(&shouting.join("app")).is_err(), "APFS: the same folder under another case");
        }
        assert_eq!(project_root(&trusted.join("app")).unwrap(), trusted.join("app"));
    }

    #[test]
    fn workspace_isolation_merges_over_the_user_config() {
        let workspace: Workspace =
            toml::from_str("projects = []\n[isolation]\nmode = 'bare'\n[isolation.sandbox]\ncpus = 2").unwrap();
        let mut user = crate::isolation::IsolationConfig::default();
        user.sandbox.memory = Some("8g".into());
        let merged = user.with_overrides(&workspace.isolation().unwrap().unwrap()).unwrap();
        assert_eq!(merged.mode, crate::isolation::IsolationMode::Bare);
        assert_eq!((merged.sandbox.memory.as_deref(), merged.sandbox.cpus), (Some("8g"), Some(2)));
        assert!(Workspace::default().isolation().unwrap().is_none());
        let broken: Workspace = toml::from_str("projects = []\nisolation = 'bare'").unwrap();
        assert!(broken.isolation().is_err());
    }

    #[test]
    fn a_workspace_sets_how_its_chief_starts() {
        let workspace: Workspace = toml::from_str(
            "projects = []\n[chief]\nprompt = 'x'\ncwd = '~/Projects'\nresume = 'session-id'\nautostart = true\n",
        )
        .unwrap();
        let chief = workspace.chief().unwrap().unwrap();
        assert_eq!(chief.cwd.as_deref(), Some("~/Projects"));
        assert_eq!(chief.resume.as_deref(), Some("session-id"));
        assert_eq!(chief.autostart, Some(true));
        let plain: Workspace = toml::from_str("projects = []\n").unwrap();
        assert!(plain.chief().unwrap().is_none());
    }

    #[test]
    fn workspace_names_become_file_names() {
        assert_eq!(file_name(" Daily work ").unwrap(), "Daily-work");
        assert!(file_name("  ").is_err());
    }

    fn init_repo(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .arg("-C")
                    .arg(path)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "base"]);
        project_root(path).unwrap()
    }

    #[test]
    fn discover_projects_finds_depth_one_and_two_dedupes_and_skips_ignored_and_untrusted() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Distrust persists for the process and matches by inode: keep this folder alive.
        std::mem::forget(dir);
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();

        let top = init_repo(&work.join("top-repo"));
        let nested_a = init_repo(&work.join("repos/app"));
        let nested_b = init_repo(&work.join("repos/service"));
        // Depth 3 must not be discovered at default depth 2.
        let _too_deep = init_repo(&work.join("repos/group/too-deep"));
        // Hidden and build/dependency folders must be skipped.
        let _hidden = init_repo(&work.join(".hidden-repo"));
        let _hidden_group = init_repo(&work.join(".secret/repo"));
        let _in_target = init_repo(&work.join("target/generated"));
        let _in_node_modules = init_repo(&work.join("node_modules/pkg"));
        let _in_dist = init_repo(&work.join("dist/bundle"));
        // A broken/empty `.git` directory is not a valid repository and is ignored.
        std::fs::create_dir_all(work.join("fake-repo/.git")).unwrap();
        // Symlink aliases (to a repo, to a container folder, and a cycle) plus a linked worktree.
        std::os::unix::fs::symlink(work.join("repos/app"), work.join("current")).unwrap();
        std::os::unix::fs::symlink(work.join("repos"), work.join("repos-alias")).unwrap();
        std::os::unix::fs::symlink(&work, work.join("loop-back")).unwrap();
        std::os::unix::fs::symlink(work.join("does-not-exist"), work.join("broken-link")).unwrap();
        assert!(
            Command::new("git")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .arg("-C")
                .arg(&nested_b)
                .args(["worktree", "add", "-q", "-b", "feature"])
                .arg(work.join("service-feature"))
                .arg("main")
                .status()
                .unwrap()
                .success()
        );
        // Untrusted folder and symlink into it must be skipped without running git.
        let untrusted_dir = root.join("vm-exchange");
        let _untrusted_repo = init_repo(&untrusted_dir.join("repo"));
        let untrusted_inside = work.join("exchange");
        let _untrusted_inside_repo = init_repo(&untrusted_inside.join("inner"));
        distrust(&untrusted_dir, "written by something else");
        distrust(&untrusted_inside, "written by something else");
        std::os::unix::fs::symlink(untrusted_dir.join("repo"), work.join("untrusted-link")).unwrap();

        let mut expected = vec![top, nested_a, nested_b];
        expected.sort();
        let discovered = discover_projects(&work, DEFAULT_DISCOVERY_DEPTH);
        assert_eq!(discovered, expected);
        assert!(discover_projects(&untrusted_dir, DEFAULT_DISCOVERY_DEPTH).is_empty());
        assert!(discover_projects(&work.join("top-repo/.git"), DEFAULT_DISCOVERY_DEPTH).is_empty());
    }

    #[test]
    fn root_home_and_folders_above_home_are_never_working_folders() {
        let home = paths::home();
        assert!(!can_discover(Path::new("/")));
        assert!(!can_discover(&home));
        assert!(!can_discover(&home.join(".")));
        if let Some(above) = home.parent() {
            assert!(!can_discover(above));
        }
        let dir = crate::test_support::tempdir().unwrap();
        assert!(can_discover(dir.path()));
    }
}
