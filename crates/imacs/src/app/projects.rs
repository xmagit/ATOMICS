use super::*;

impl App {
    /// `imacs <path>` or `--workspace`, else the repository IMACS was started in, else
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

    pub(super) fn pick_project(&mut self) {
        let start = self.repos.first().and_then(|r| r.parent()).map(Path::to_path_buf).unwrap_or_else(paths::home);
        if let Some(dir) = rfd::FileDialog::new().set_title("Open project").set_directory(start).pick_folder() {
            self.open_project(&dir);
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
        let ws = Workspace { name: name.trim().to_string(), projects: self.open_projects.clone() };
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
