//! Projects and workspaces. A project is one git repository, always shown through
//! its main checkout. A workspace is a named set of projects, saved as
//! `~/.config/imacs/workspaces/<name>.toml`.

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
    /// This workspace's ticket system; the main config's `[tickets]` applies without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tickets: Option<crate::tickets::TicketConfig>,
}

#[cfg(feature = "native")]
pub fn workspaces_dir() -> PathBuf {
    paths::config_file().parent().map(Path::to_path_buf).unwrap_or_else(paths::imacs_home).join("workspaces")
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
        Self::load_file(name, &Self::file(name)?)
    }

    pub fn file(name: &str) -> Result<PathBuf> {
        Ok(workspaces_dir().join(format!("{}.toml", file_name(name)?)))
    }

    fn load_file(name: &str, file: &Path) -> Result<Workspace> {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let mut workspace: Workspace = toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        workspace.name = name.to_string();
        workspace.projects = workspace.projects.iter().map(|p| paths::expand(&p.to_string_lossy())).collect();
        Ok(workspace)
    }

    /// Writes the project list, keeping everything else in an existing file as written.
    pub fn save(&self) -> Result<PathBuf> {
        let file = Self::file(&self.name)?;
        std::fs::create_dir_all(workspaces_dir())?;
        self.save_file(&file)?;
        Ok(file)
    }

    fn save_file(&self, file: &Path) -> Result<()> {
        let mut document = match std::fs::read_to_string(file) {
            Ok(text) => text
                .parse::<toml_edit::DocumentMut>()
                .with_context(|| format!("parsing {}; fix or remove it first", file.display()))?,
            Err(_) => toml_edit::DocumentMut::new(),
        };
        let shown: toml_edit::Array = self.projects.iter().map(|p| paths::display(p)).collect();
        document["projects"] = toml_edit::value(shown);
        std::fs::write(file, document.to_string())?;
        Ok(())
    }

    /// Names of the saved workspaces, sorted.
    pub fn list() -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(workspaces_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .filter(|_| e.path().extension().is_some_and(|x| x == "toml"))
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
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
    fn saving_keeps_the_hand_written_tickets_section() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("daily.toml");
        let workspace = Workspace { name: "daily".into(), projects: vec!["/repos/app".into()], tickets: None };
        workspace.save_file(&file).unwrap();
        assert_eq!(Workspace::load_file("daily", &file).unwrap(), workspace);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(
            &file,
            format!("{text}\n# The team's tracker.\n[tickets]\nsystem = \"alpha\" # ours\nserver = {{ command = [\"alpha-mcp\"] }}\n"),
        )
        .unwrap();
        let loaded = Workspace::load_file("daily", &file).unwrap();
        assert_eq!(loaded.tickets.as_ref().unwrap().system, "alpha");
        Workspace { projects: vec!["/repos/app".into(), "/repos/api".into()], ..loaded.clone() }
            .save_file(&file)
            .unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("# The team's tracker.") && text.contains("# ours"), "{text}");
        let reloaded = Workspace::load_file("daily", &file).unwrap();
        assert_eq!(reloaded.projects.len(), 2);
        assert_eq!(reloaded.tickets, loaded.tickets);
        std::fs::write(&file, "projects = [").unwrap();
        assert!(workspace.save_file(&file).is_err(), "an unreadable file is never overwritten");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "projects = [");
    }

    #[test]
    fn workspace_names_become_file_names() {
        assert_eq!(file_name(" Daily work ").unwrap(), "Daily-work");
        assert!(file_name("  ").is_err());
    }
}
