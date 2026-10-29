//! Projects and workspaces. A project is one git repository, always shown through
//! its main checkout. A workspace is a named set of projects, saved as
//! `~/.config/imacs/workspaces/<host>/<name>.toml`.

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
    /// This workspace's ticket system, read from its `[tickets]`. The section stays in
    /// `extra` as written, so saving never drops, rewrites or duplicates it.
    pub fn tickets(&self) -> Result<Option<crate::tickets::TicketConfig>, toml::de::Error> {
        self.extra.get("tickets").map(|section| section.clone().try_into()).transpose()
    }

    pub fn workflow(&self) -> Result<Option<crate::workflow::Workflow>, toml::de::Error> {
        self.extra.get("workflow").map(|section| section.clone().try_into()).transpose()
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
    paths::config_file().parent().map(Path::to_path_buf).unwrap_or_else(paths::imacs_home).join("workspaces")
}

#[cfg(feature = "native")]
pub fn workspaces_dir() -> PathBuf {
    workspaces_root().join(paths::host_name())
}

/// The project a folder belongs to: the main checkout of its repository, so a
/// folder inside any worktree opens the same project.
#[cfg(feature = "native")]
pub fn project_root(dir: &Path) -> Result<PathBuf> {
    let worktree =
        git::worktree_of(dir).with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    let main = git::list_worktrees(&worktree.root).ok().and_then(|list| list.into_iter().next()).map(|w| w.path);
    Ok(main.unwrap_or(worktree.root))
}

#[cfg(feature = "native")]
impl Workspace {
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
        const CHILD: &str = "IMACS_LEGACY_WORKSPACE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "workspace::tests::legacy_workspaces_load_list_and_save_without_modifying_shared_files",
                    ])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("IMACS_HOST", "work-mac")
                    .env("IMACS_CONFIG", dir.path().join("config/config.toml")),
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
        let legacy_file = root.join("cz3backend.toml");
        let legacy_text = "projects = ['~/repos/cz3backend']\n[tickets]\nprovider = 'linear'\n";
        std::fs::write(&legacy_file, legacy_text).unwrap();
        assert_eq!(Workspace::list(), ["cz3backend"]);
        let mut workspace = Workspace::load("cz3backend").unwrap();
        assert_eq!(workspace.projects, [paths::home().join("repos/cz3backend")]);
        workspace.projects = vec![paths::home().join("repos/host-project")];
        let host_file = workspace.save().unwrap();
        assert_eq!(host_file, workspaces_dir().join("cz3backend.toml"));
        assert_eq!(std::fs::read_to_string(&legacy_file).unwrap(), legacy_text);
        assert_eq!(Workspace::list(), ["cz3backend"]);
        let host_workspace = Workspace::load("cz3backend").unwrap();
        assert_eq!(host_workspace.projects, workspace.projects);
        assert_eq!(host_workspace.extra, workspace.extra);

        std::fs::write(root.join("legacy-only.toml"), "projects = []").unwrap();
        std::fs::write(workspaces_dir().join("host-only.toml"), "projects = []").unwrap();
        std::fs::create_dir(root.join("other-host")).unwrap();
        std::fs::write(root.join("other-host/other.toml"), "projects = []").unwrap();
        assert_eq!(Workspace::list(), ["cz3backend", "host-only", "legacy-only"]);
        // An invalid host file must not silently load a different workspace.
        std::fs::write(&host_file, "[broken").unwrap();
        let error = format!("{:#}", Workspace::load("cz3backend").unwrap_err());
        assert!(error.contains(&host_file.display().to_string()), "{error}");
        assert_eq!(std::fs::read_to_string(&legacy_file).unwrap(), legacy_text);
        std::fs::remove_file(host_file).unwrap();
        assert_eq!(Workspace::load("cz3backend").unwrap().projects, [paths::home().join("repos/cz3backend")]);
    }

    #[test]
    fn workspaces_are_host_local_and_preserve_portable_paths_and_ticket_settings() {
        const CHILD: &str = "IMACS_WORKSPACE_HOST_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
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
                        .env(CHILD, "1").env("HOME", dir.path()).env("IMACS_HOST", host)
                        .env("IMACS_CONFIG", dir.path().join("config/config.toml")),
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
        let host = std::env::var("IMACS_HOST").unwrap();
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
        const CHILD: &str = "IMACS_WORKSPACE_TICKETS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "workspace::tests::saving_keeps_one_tickets_section_that_still_parses"])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("IMACS_HOST", "work-mac")
                    .env("IMACS_CONFIG", dir.path().join("config/config.toml")),
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
        let dir = tempfile::tempdir().unwrap();
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
    fn workspace_names_become_file_names() {
        assert_eq!(file_name(" Daily work ").unwrap(), "Daily-work");
        assert!(file_name("  ").is_err());
    }
}
