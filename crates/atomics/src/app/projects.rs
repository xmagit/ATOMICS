use super::*;

impl State {
    /// `atomics <path>` or `--workspace`, else the repository ATOMICS was started in, else
    /// the projects from last time.
    pub(super) fn open_initial_projects(&mut self, requested: Vec<PathBuf>, workspace: Option<String>) {
        let mut projects = requested;
        if let Some(name) = &workspace {
            match Workspace::load(name) {
                Ok(ws) => projects.extend(ws.projects),
                Err(err) => self.toast(format!("{err:#}")),
            }
        }
        if projects.is_empty() && workspace::project_root(&self.launch_dir).is_ok() {
            projects.push(self.launch_dir.clone());
        }
        if projects.is_empty() {
            projects = self.store.projects.clone();
        }
        for dir in projects {
            if let Ok(root) = workspace::project_root(&dir)
                && !self.open_projects.contains(&root)
            {
                self.open_projects.push(root);
            }
        }
        self.remember_projects();
    }

    fn remember_projects(&mut self) {
        for root in self.open_projects.clone() {
            self.store.touch_project(&root);
        }
        self.store.projects = self.open_projects.clone();
        let _ = self.store.save(&paths::store_file());
    }

    pub(super) fn open_project(&mut self, dir: &Path) {
        match workspace::project_root(dir) {
            Ok(root) => {
                if !self.open_projects.contains(&root) {
                    self.open_projects.push(root.clone());
                }
                self.remember_projects();
                self.apply_config();
                self.active = None;
                self.diff_root = Some(root.clone());
                self.diff_open = true;
                self.toast(format!("Opened {}", paths::display(&root)));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    pub(super) fn close_project(&mut self, root: &Path) {
        self.open_projects.retain(|p| p != root);
        self.remember_projects();
        self.apply_config();
        if self.diff_root.as_deref().is_some_and(|d| self.project_of(d).as_deref() == Some(root) || d == root) {
            self.diff_root = None;
        }
    }

    pub(super) fn open_workspace(&mut self, name: &str) {
        match Workspace::load(name) {
            Ok(ws) => {
                self.open_projects = ws.projects.iter().filter_map(|p| workspace::project_root(p).ok()).collect();
                self.workspace = Some(name.to_string());
                self.remember_projects();
                self.apply_config();
                self.active = None;
                self.diff_root = self.open_projects.first().cloned();
                self.toast(format!("Workspace {name}: {} projects", self.open_projects.len()));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    pub(super) fn save_workspace(&mut self, name: &str) {
        let mut ws = match Workspace::load(name) {
            Ok(ws) => ws,
            Err(err)
                if err.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Workspace::default()
            }
            Err(err) => {
                self.toast(format!("{err:#}"));
                return;
            }
        };
        ws.name = name.trim().to_string();
        ws.projects = self.open_projects.clone();
        match ws.save() {
            Ok(file) => {
                self.workspace = Some(ws.name);
                self.toast(format!("Saved {}", paths::display(&file)));
            }
            Err(err) => self.toast(format!("{err:#}")),
        }
    }

    /// The open project a folder belongs to.
    pub(super) fn project_of(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(root) = self.repos.iter().find(|r| dir.starts_with(r)) {
            return Some(root.clone());
        }
        self.worktree_project
            .iter()
            .filter(|(worktree, _)| dir.starts_with(worktree))
            .max_by_key(|(worktree, _)| worktree.as_os_str().len())
            .map(|(_, project)| project.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::with_state;

    #[test]
    fn saving_project_changes_keeps_workspace_ticket_settings() {
        with_state("app::projects::tests::saving_project_changes_keeps_workspace_ticket_settings", |mut state, _| {
            let dir = workspace::workspaces_dir();
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("daily.toml");
            std::fs::write(&file, "projects = []\n[tickets]\nprovider = 'relay'\nworkflow = 'personal'\n").unwrap();
            state.open_projects = vec![paths::home().join("repos/app")];
            state.save_workspace("daily");
            let saved = Workspace::load("daily").unwrap();
            assert_eq!(saved.projects, state.open_projects);
            assert_eq!(saved.extra["tickets"]["workflow"].as_str(), Some("personal"));
            assert!(std::fs::read_to_string(&file).unwrap().contains("~/repos/app"));

            std::fs::write(&file, "[broken").unwrap();
            state.save_workspace("daily");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "[broken");
            assert!(state.toast.as_ref().unwrap().0.contains("parsing"));
        });
    }
}
