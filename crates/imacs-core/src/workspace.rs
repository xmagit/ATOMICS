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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(skip)]
    pub name: String,
    pub projects: Vec<PathBuf>,
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
        let file = workspaces_dir().join(format!("{}.toml", file_name(name)?));
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let mut workspace: Workspace = toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        workspace.name = name.to_string();
        workspace.projects = workspace.projects.iter().map(|p| paths::expand(&p.to_string_lossy())).collect();
        Ok(workspace)
    }

    pub fn save(&self) -> Result<PathBuf> {
        let dir = workspaces_dir();
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{}.toml", file_name(&self.name)?));
        let shown: Vec<String> = self.projects.iter().map(|p| paths::display(p)).collect();
        std::fs::write(&file, toml::to_string(&toml::toml! { projects = shown })?)?;
        Ok(file)
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
    fn workspace_names_become_file_names() {
        assert_eq!(file_name(" Daily work ").unwrap(), "Daily-work");
        assert!(file_name("  ").is_err());
    }
}
